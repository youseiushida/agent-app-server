//! One WebSocket connection.
//!
//! Tasks per connection:
//! * reader — parses frames, dispatches requests (via [`Lanes`]), and closes the connection
//!   `client_timeout` after its last inbound frame ([`InboundDeadline`]);
//! * writer — owns the socket sink; drains the high-priority queue before stream batches;
//! * heartbeat — sends `heartbeat` + Ping every interval;
//! * one tailer per subscription — reads the log from its cursor and waits on the head.

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use aas_core::{Batch, CoreResult, Engine, RequestCtx};
use aas_protocol::methods::*;
use aas_protocol::notifications::*;
use aas_protocol::rpc::{MessageKind, RequestId, RpcMessage};
use aas_protocol::*;
use axum::extract::ws::{CloseFrame, Message, Utf8Bytes, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, State};
use axum::http::HeaderMap;
use axum::response::Response;
use bytes::Bytes;
use futures::{Sink, SinkExt, Stream, StreamExt};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;

use crate::errors::Api;
use crate::lanes::{Lanes, key_of};
use crate::{AppState, CloseReason, ConnEntry, ShutdownNotice};

const CLOSE_REPLACED: u16 = 4000;
const CLOSE_REVOKED: u16 = 4001;
const CLOSE_TIMEOUT: u16 = 4002;
const CLOSE_PROTOCOL: u16 = 4003;
const CLOSE_GOING_AWAY: u16 = 1001;
/// RFC 6455 "Message Too Big": a frame above `policy.max_transport_frame_bytes`.
const CLOSE_TOO_BIG: u16 = 1009;

pub(crate) async fn upgrade(
    State(state): State<Arc<AppState>>,
    Api(ConnectInfo(peer)): Api<ConnectInfo<std::net::SocketAddr>>,
    headers: HeaderMap,
    Api(ws): Api<WebSocketUpgrade>,
) -> Response {
    let device = match crate::http::authenticate(&state, &headers).await {
        Ok(d) => d,
        Err(e) => return axum::response::IntoResponse::into_response(e),
    };
    // Frames up to the transport limit are read, so that a request above
    // `max_client_frame_bytes` can still be answered (see `Conn::on_text`).
    let max = state.engine.policy().max_transport_frame_bytes;
    tracing::info!(device = %device.id, name = %device.name, %peer, "websocket connected");
    // Counted from here on, before the upgrade completes: a shutdown waits for this connection
    // to deliver `server/shuttingDown` and its close frame (see `Server::run_until`).
    let tracked = state.connection_tasks.token();
    ws.max_message_size(max)
        .max_frame_size(max)
        .on_upgrade(move |socket| async move {
            run(state, device, socket).await;
            drop(tracked);
        })
}

#[derive(Debug)]
enum Out {
    Text(String),
    Ping,
    Close(u16, &'static str),
}

struct Conn {
    state: Arc<AppState>,
    device: aas_core::AuthenticatedDevice,
    hi: mpsc::UnboundedSender<Out>,
    lo: mpsc::Sender<Out>,
    initialized: bool,
    tailers: HashMap<String, JoinHandle<()>>,
    lanes: Lanes,
    /// Subscribed streams, with the latest head a tailer has read (fallback for heartbeats
    /// when the log cannot be read).
    heads: Heads,
}

type Heads = Arc<parking_lot::Mutex<BTreeMap<String, u64>>>;

/// Wall-clock time for the `serverTime` of heartbeats (shown to the user, never used to
/// measure a duration).
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// The deadline of a connection's silence: `client_timeout` after its last inbound frame
/// (a Pong counts), re-armed on every frame. The connection closes exactly when it passes, not
/// at the next heartbeat tick after it (checking on the ticks made the effective limit anything
/// up to `client_timeout + heartbeat_interval`).
///
/// Measured on the monotonic clock (`tokio::time::Instant`). The wall clock can step (a time
/// synchronization, the user changing the time, a correction after resuming from sleep):
/// measured on it, one step forward would close every connection at once and one step back
/// would keep a dead connection open until the clock catches up.
struct InboundDeadline {
    timeout: Duration,
    last_frame: tokio::time::Instant,
    timer: Pin<Box<tokio::time::Sleep>>,
}

impl InboundDeadline {
    /// Armed from now (the connection's opening counts as its first frame).
    fn new(timeout: Duration) -> Self {
        let now = tokio::time::Instant::now();
        Self {
            timeout,
            last_frame: now,
            timer: Box::pin(tokio::time::sleep_until(now + timeout)),
        }
    }

    /// A frame arrived: the deadline moves to `timeout` from now.
    fn touch(&mut self) {
        let now = tokio::time::Instant::now();
        self.last_frame = now;
        self.timer.as_mut().reset(now + self.timeout);
    }

    /// How long no frame has arrived.
    fn silent_for(&self) -> Duration {
        self.last_frame.elapsed()
    }

    /// Resolves once `timeout` has passed since the last frame.
    async fn expired(&mut self) {
        self.timer.as_mut().await;
    }
}

/// Where heartbeats read the heads of the subscribed streams.
trait HeadSource: Send + Sync + 'static {
    fn stream_heads(
        &self,
        streams: Vec<String>,
    ) -> impl Future<Output = CoreResult<BTreeMap<String, u64>>> + Send;
}

impl HeadSource for Engine {
    fn stream_heads(
        &self,
        streams: Vec<String>,
    ) -> impl Future<Output = CoreResult<BTreeMap<String, u64>>> + Send {
        Engine::stream_heads(self, streams)
    }
}

/// The close code for a failed read of the socket (`None`: the peer is gone, nothing to send).
fn close_for_read_error(error: axum::Error) -> Option<(u16, &'static str)> {
    let inner = error.into_inner();
    match inner.downcast_ref::<tungstenite::Error>() {
        Some(tungstenite::Error::Capacity(_)) => Some((CLOSE_TOO_BIG, "message too big")),
        _ => {
            tracing::debug!(error = %inner, "websocket read failed");
            None
        }
    }
}

async fn run(state: Arc<AppState>, device: aas_core::AuthenticatedDevice, socket: WebSocket) {
    let (replace_tx, replace_rx) = oneshot::channel();
    let conn_id = state.next_conn_id.fetch_add(1, Ordering::SeqCst);
    if let Some(old) = state.connections.lock().insert(
        device.id.clone(),
        ConnEntry {
            id: conn_id,
            replace: replace_tx,
        },
    ) {
        let _ = old.replace.send(CloseReason::Replaced);
    }
    // The device was authenticated before the upgrade. A revocation committed since then
    // found no connection to close; now that this one is registered (and visible to the
    // revocation watch), check again.
    let revoked = match state.engine.device_active(&device.id).await {
        Ok(active) => !active,
        Err(e) => {
            tracing::warn!(device = %device.id, error = %e, "could not re-check the device; keeping the connection");
            false
        }
    };
    if !revoked && let Err(e) = state.engine.touch_device(&device.id).await {
        tracing::warn!(error = %e, "could not record device activity");
    }

    let (sink, mut stream) = socket.split();
    let (hi_tx, hi_rx) = mpsc::unbounded_channel();
    let (lo_tx, mut lo_rx) = mpsc::channel(state.policy.stream_batch_queue);
    let (done_tx, done_rx) = oneshot::channel();
    let lo_stream = futures::stream::poll_fn(move |cx| lo_rx.poll_recv(cx));
    let mut writer = tokio::spawn(write_loop(sink, hi_rx, lo_stream, done_rx));
    let policy = state.engine.policy().clone();
    let mut inbound = InboundDeadline::new(policy.client_timeout);
    let heads: Heads = Arc::default();
    let mut conn = Conn {
        state: state.clone(),
        device: device.clone(),
        hi: hi_tx.clone(),
        lo: lo_tx,
        initialized: false,
        tailers: HashMap::new(),
        lanes: Lanes::default(),
        heads: heads.clone(),
    };

    let heartbeat = tokio::spawn(heartbeat_loop(
        state.engine.clone(),
        heads,
        hi_tx.clone(),
        policy.heartbeat_interval,
    ));
    let mut shutdown_rx = state.shutdown_rx.clone();
    let mut replace_rx = replace_rx;

    let close = if revoked {
        Some((CLOSE_REVOKED, "device revoked"))
    } else {
        loop {
            tokio::select! {
                frame = stream.next() => {
                    let frame = match frame {
                        Some(Ok(frame)) => frame,
                        Some(Err(e)) => break close_for_read_error(e),
                        None => break None,
                    };
                    inbound.touch();
                    match frame {
                        Message::Text(text) => conn.on_text(text.as_str()).await,
                        Message::Binary(_) => break Some((CLOSE_PROTOCOL, "binary frames are not supported")),
                        Message::Close(_) => break None,
                        Message::Ping(_) | Message::Pong(_) => {}
                    }
                }
                reason = &mut replace_rx => {
                    match reason.unwrap_or(CloseReason::Replaced) {
                        CloseReason::Replaced => {
                            conn.notify(ServerNotification::ConnectionReplaced(ConnectionReplaced {}));
                            break Some((CLOSE_REPLACED, "replaced by a newer connection"));
                        }
                        CloseReason::Revoked => break Some((CLOSE_REVOKED, "device revoked")),
                    }
                }
                _ = inbound.expired() => {
                    tracing::info!(
                        device = %device.id,
                        silent_ms = inbound.silent_for().as_millis() as u64,
                        "client timed out"
                    );
                    break Some((CLOSE_TIMEOUT, "no frames within client_timeout"));
                }
                notice = async { shutdown_rx.wait_for(Option::is_some).await.ok().and_then(|n| *n) } => {
                    // The notice was decided once by whoever stopped the server; every
                    // connection reports the same reason. A server dropped without a notice
                    // (its task ended) is an ordinary shutdown that nothing announced a
                    // restart for.
                    let notice = notice.unwrap_or(ShutdownNotice { reason: ShutdownReason::Shutdown, restart_expected: false });
                    conn.notify(ServerNotification::ServerShuttingDown(ServerShuttingDown {
                        reason: notice.reason,
                        restart_expected: notice.restart_expected,
                    }));
                    break Some((CLOSE_GOING_AWAY, "server shutting down"));
                }
            }
        }
    };

    for (_, t) in conn.tailers.drain() {
        t.abort();
    }
    conn.lanes.close();
    heartbeat.abort();
    if let Some((code, reason)) = close {
        let _ = conn.hi.send(Out::Close(code, reason));
    }
    // Requests still running hold senders of the high-priority queue; the writer ends after
    // what is queued now instead of waiting for them.
    let _ = done_tx.send(());
    drop(conn);
    drop(hi_tx);
    if tokio::time::timeout(state.policy.writer_flush_timeout, &mut writer)
        .await
        .is_err()
    {
        writer.abort();
    }
    {
        let mut conns = state.connections.lock();
        if conns.get(&device.id).is_some_and(|c| c.id == conn_id) {
            conns.remove(&device.id);
        }
    }
    tracing::info!(device = %device.id, close = ?close.map(|c| c.1), "websocket closed");
}

/// Owns the sink. High-priority messages always go first; stream batches fill the gaps. Ends
/// on a close message, when `done` fires (the connection is over: whatever is still queued is
/// sent first), or when the socket fails.
async fn write_loop<S, L>(
    mut sink: S,
    mut hi: mpsc::UnboundedReceiver<Out>,
    mut lo: L,
    mut done: oneshot::Receiver<()>,
) where
    S: Sink<Message> + Unpin,
    L: Stream<Item = Out> + Unpin,
{
    // Once the batch queue has closed it must not be polled again (it would be ready
    // immediately, forever).
    let mut lo_open = true;
    let mut finishing = false;
    loop {
        let out = tokio::select! {
            biased;
            m = hi.recv() => match m {
                Some(m) => m,
                None => break,
            },
            _ = &mut done, if !finishing => {
                // Deliver what is already queued (e.g. the close frame), then stop.
                if hi.is_empty() {
                    break;
                }
                finishing = true;
                continue;
            }
            m = lo.next(), if lo_open && !finishing => match m {
                Some(m) => m,
                None => {
                    lo_open = false;
                    continue;
                }
            },
        };
        let result = match out {
            Out::Text(t) => sink.send(Message::Text(Utf8Bytes::from(t))).await,
            Out::Ping => sink.send(Message::Ping(Bytes::new())).await,
            Out::Close(code, reason) => {
                let _ = sink
                    .send(Message::Close(Some(CloseFrame {
                        code,
                        reason: Utf8Bytes::from(reason),
                    })))
                    .await;
                break;
            }
        };
        if result.is_err() {
            break;
        }
        if finishing && hi.is_empty() {
            break;
        }
    }
    let _ = sink.close().await;
}

/// Sends a heartbeat and a Ping every `interval`. (A silent client is closed by the reader's
/// [`InboundDeadline`], independently of these ticks.)
async fn heartbeat_loop<L: HeadSource>(
    engine: Arc<L>,
    heads: Heads,
    hi: mpsc::UnboundedSender<Out>,
    interval: Duration,
) {
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    ticker.tick().await;
    loop {
        ticker.tick().await;
        // The heads come from the log itself, not from the tailers: a subscription whose
        // tailer stalled shows a head ahead of the client's cursor, and the client
        // resubscribes (design.md §7.3).
        let (streams, noted) = {
            let heads = heads.lock();
            (heads.keys().cloned().collect::<Vec<_>>(), heads.clone())
        };
        let heads = match engine.stream_heads(streams).await {
            Ok(h) => h,
            Err(e) => {
                tracing::warn!(error = %e, "reading stream heads failed; reporting the heads last read");
                noted
            }
        };
        let note = ServerNotification::Heartbeat(Heartbeat {
            server_time: now_ms(),
            heads,
        });
        let msg = RpcMessage::notification(note.method(), note.params_json());
        if hi
            .send(Out::Text(serde_json::to_string(&msg).expect("serializes")))
            .is_err()
            || hi.send(Out::Ping).is_err()
        {
            return;
        }
    }
}

impl Conn {
    fn ctx(&self) -> RequestCtx {
        RequestCtx {
            device_id: self.device.id.clone(),
        }
    }

    fn notify(&self, note: ServerNotification) {
        let msg = RpcMessage::notification(note.method(), note.params_json());
        let _ = self
            .hi
            .send(Out::Text(serde_json::to_string(&msg).expect("serializes")));
    }

    fn respond(hi: &mpsc::UnboundedSender<Out>, id: RequestId, result: Result<Value, RpcError>) {
        let msg = match result {
            Ok(v) => RpcMessage::response_ok(id, v),
            Err(e) => RpcMessage::response_err(Some(id), e),
        };
        let _ = hi.send(Out::Text(serde_json::to_string(&msg).expect("serializes")));
    }

    async fn on_text(&mut self, text: &str) {
        let max = self.state.engine.policy().max_client_frame_bytes;
        if text.len() > max {
            // Answer definitively (with the request's id when it has one), so the client drops
            // the request instead of resending it after every reconnect.
            #[derive(serde::Deserialize)]
            struct IdOnly {
                #[serde(default)]
                id: Option<RequestId>,
            }
            let id = serde_json::from_str::<IdOnly>(text).ok().and_then(|m| m.id);
            let err = RpcError::new(
                ErrorKind::PayloadTooLarge,
                format!(
                    "the message has {} bytes; the limit is {max} (send large data as a blob)",
                    text.len()
                ),
            )
            .with("maxClientFrameBytes", max as u64);
            tracing::info!(bytes = text.len(), max, "rejected an oversized request");
            let _ = self.hi.send(Out::Text(
                serde_json::to_string(&RpcMessage::response_err(id, err)).expect("serializes"),
            ));
            return;
        }
        let msg: RpcMessage = match serde_json::from_str(text) {
            Ok(m) => m,
            Err(e) => {
                let err = RpcError::new(ErrorKind::ParseError, format!("invalid JSON: {e}"));
                let _ = self.hi.send(Out::Text(
                    serde_json::to_string(&RpcMessage::response_err(None, err))
                        .expect("serializes"),
                ));
                return;
            }
        };
        match msg.kind() {
            MessageKind::Request => {}
            MessageKind::Notification => {
                tracing::debug!(method = ?msg.method, "ignoring client notification");
                return;
            }
            MessageKind::Response | MessageKind::Invalid => {
                let err =
                    RpcError::new(ErrorKind::InvalidRequest, "expected a JSON-RPC 2.0 request");
                let _ = self.hi.send(Out::Text(
                    serde_json::to_string(&RpcMessage::response_err(msg.id, err))
                        .expect("serializes"),
                ));
                return;
            }
        }
        let id = msg.id.clone().expect("request has id");
        let method = msg.method.clone().expect("request has method");
        let req = match ClientRequest::parse(&method, msg.params) {
            Ok(r) => r,
            Err(e) => return Self::respond(&self.hi, id, Err(e)),
        };
        match req {
            ClientRequest::Initialize(p) => {
                let r = self.initialize(p);
                Self::respond(&self.hi, id, r);
            }
            _ if !self.initialized => {
                Self::respond(
                    &self.hi,
                    id,
                    Err(RpcError::new(
                        ErrorKind::NotInitialized,
                        "call initialize first",
                    )),
                );
            }
            ClientRequest::Subscribe(p) => {
                let r = self.subscribe(p).await;
                Self::respond(&self.hi, id, r);
            }
            ClientRequest::Unsubscribe(p) => {
                for s in p.streams {
                    if let Some(t) = self.tailers.remove(&s) {
                        t.abort();
                    }
                    self.heads.lock().remove(&s);
                }
                Self::respond(&self.hi, id, Ok(serde_json::json!({})));
            }
            other => {
                let engine = self.state.engine.clone();
                let ctx = self.ctx();
                let hi = self.hi.clone();
                let key = key_of(&other);
                self.lanes.submit(key, async move {
                    let result = engine.handle(&ctx, other).await;
                    Conn::respond(&hi, id, result);
                });
            }
        }
    }

    fn initialize(&mut self, p: InitializeParams) -> Result<Value, RpcError> {
        if p.protocol_version != PROTOCOL_VERSION {
            return Err(RpcError::new(
                ErrorKind::ProtocolVersionUnsupported,
                format!(
                    "server speaks protocol {PROTOCOL_VERSION}, client {}",
                    p.protocol_version
                ),
            )
            .with("supported", vec![PROTOCOL_VERSION]));
        }
        self.initialized = true;
        let engine = &self.state.engine;
        let result = InitializeResult {
            protocol_version: PROTOCOL_VERSION,
            server: engine.server_info(),
            device: DeviceInfo {
                id: self.device.id.clone(),
                name: self.device.name.clone(),
            },
            epoch_changed: p.last_known_epoch.is_some_and(|e| e != engine.epoch()),
            policy: engine.policy().client_policy(),
        };
        tracing::debug!(client = %p.client.name, version = %p.client.version, "initialized");
        Ok(serde_json::to_value(result).expect("serializes"))
    }

    async fn subscribe(&mut self, p: SubscribeParams) -> Result<Value, RpcError> {
        let mut statuses = Vec::with_capacity(p.subscriptions.len());
        for sub in p.subscriptions {
            let head = self
                .state
                .engine
                .stream_head(&sub.stream)
                .await
                .map_err(RpcError::from)?;
            let Some(head) = head else {
                statuses.push(SubscriptionStatus {
                    stream: sub.stream,
                    head: 0,
                    status: SubscriptionState::NotFound,
                });
                continue;
            };
            if let Some(old) = self.tailers.remove(&sub.stream) {
                old.abort();
            }
            // A cursor ahead of the log (a client bug, or a data folder restored from a backup)
            // starts at the head: nothing after it is lost, and the answer's `head` shows the
            // client that its cursor was ahead.
            if sub.after > head {
                tracing::warn!(stream = %sub.stream, after = sub.after, head, "subscription cursor is ahead of the stream; starting at the head");
            }
            let after = sub.after.min(head);
            self.heads.lock().insert(sub.stream.clone(), head);
            let retry = self.state.engine.policy().heartbeat_interval;
            let tailer = tokio::spawn(tail(
                self.state.engine.clone(),
                sub.stream.clone(),
                after,
                self.lo.clone(),
                self.heads.clone(),
                retry,
            ));
            self.tailers.insert(sub.stream.clone(), tailer);
            statuses.push(SubscriptionStatus {
                stream: sub.stream,
                head,
                status: SubscriptionState::Ok,
            });
        }
        Ok(serde_json::to_value(SubscribeResult {
            subscriptions: statuses,
        })
        .expect("serializes"))
    }
}

/// The event log as seen by a tailer.
trait StreamLog: Send + Sync + 'static {
    fn subscribe_head(&self, stream: &str) -> watch::Receiver<u64>;
    fn read_batch(
        &self,
        stream: String,
        after: u64,
    ) -> impl Future<Output = CoreResult<Batch>> + Send;
}

impl StreamLog for Engine {
    fn subscribe_head(&self, stream: &str) -> watch::Receiver<u64> {
        Engine::subscribe_head(self, stream)
    }

    fn read_batch(
        &self,
        stream: String,
        after: u64,
    ) -> impl Future<Output = CoreResult<Batch>> + Send {
        Engine::read_batch(self, stream, after)
    }
}

/// Delivers `stream` from `after` onwards, forever (until aborted).
///
/// A failed read is retried after `retry` (one heartbeat interval); stored data that cannot be
/// decoded ends the subscription with an error in the log — the heartbeats then show the
/// client a head ahead of its cursor, so it resubscribes and the failure stays visible.
///
/// The events after the cursor may be gone while the head stays where it was: retention
/// deletes old `native` events even at the end of a stream, and a removed thread's stream is
/// deleted with its head. A read that finds nothing while the head is ahead of the cursor
/// therefore moves the cursor to the head and tells the client with an empty batch (nothing at
/// or below that head can appear any more: the read is one snapshot, and new events get higher
/// numbers). The tailer then waits for a commit after its read; it never reads again because
/// of a head it has already read up to.
async fn tail<L: StreamLog>(
    log: Arc<L>,
    stream: String,
    after: u64,
    out: mpsc::Sender<Out>,
    heads: Heads,
    retry: Duration,
) {
    let mut cursor = after;
    let note_head = |h: u64| {
        let mut heads = heads.lock();
        if let Some(cur) = heads.get_mut(&stream) {
            *cur = (*cur).max(h);
        }
    };
    let batch_message = |head: u64, events: Vec<aas_protocol::events::EventEnvelope>| {
        let note = ServerNotification::StreamBatch(StreamBatch {
            stream: stream.clone(),
            head,
            events,
        });
        let msg = RpcMessage::notification(note.method(), note.params_json());
        Out::Text(serde_json::to_string(&msg).expect("serializes"))
    };
    loop {
        // Subscribe before reading: a commit after the read publishes after it (and marks
        // this receiver changed); a commit before it is part of what the read sees.
        let mut head_rx = log.subscribe_head(&stream);
        let batch = match log.read_batch(stream.clone(), cursor).await {
            Ok(b) => b,
            Err(e) if e.is_corrupt_data() => {
                tracing::error!(%stream, cursor, error = %e, "the event log cannot be decoded here; the subscription stopped");
                return;
            }
            Err(e) => {
                tracing::warn!(%stream, cursor, error = %e, retry_in = ?retry, "reading the event log failed; retrying");
                tokio::time::sleep(retry).await;
                continue;
            }
        };
        note_head(batch.head);
        if !batch.events.is_empty() {
            cursor = batch.last_seq;
            if out
                .send(batch_message(batch.head, batch.events))
                .await
                .is_err()
            {
                return;
            }
            continue;
        }
        if batch.head > cursor {
            // The events between the cursor and the head were deleted: the client moves on.
            cursor = batch.head;
            if out
                .send(batch_message(batch.head, Vec::new()))
                .await
                .is_err()
            {
                return;
            }
        }
        if head_rx.changed().await.is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::task::{Context, Poll};

    use aas_core::CoreError;
    use aas_protocol::events::{Event, EventEnvelope};

    /// A batch stream that is closed and counts how often it is polled.
    struct ClosedStream(Arc<AtomicUsize>);

    impl Stream for ClosedStream {
        type Item = Out;
        fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Out>> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Poll::Ready(None)
        }
    }

    /// Heads of a log that never changes.
    struct FixedHeads;

    impl HeadSource for FixedHeads {
        async fn stream_heads(&self, streams: Vec<String>) -> CoreResult<BTreeMap<String, u64>> {
            Ok(streams.into_iter().map(|s| (s, 7)).collect())
        }
    }

    fn heartbeat() -> (JoinHandle<()>, mpsc::UnboundedReceiver<Out>) {
        let (hi_tx, hi_rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(heartbeat_loop(
            Arc::new(FixedHeads),
            Arc::default(),
            hi_tx,
            Duration::from_secs(15),
        ));
        (task, hi_rx)
    }

    /// Whether the deadline has passed by now (polled once, without waiting).
    async fn has_expired(deadline: &mut InboundDeadline) -> bool {
        tokio::time::timeout(Duration::ZERO, deadline.expired())
            .await
            .is_ok()
    }

    #[tokio::test(start_paused = true)]
    async fn a_silent_connection_expires_exactly_client_timeout_after_its_last_frame() {
        // Only the monotonic clock moves here (tokio's paused clock); the wall clock of the
        // machine does not take part in the decision at all.
        let timeout = Duration::from_secs(45);
        let mut deadline = InboundDeadline::new(timeout);
        tokio::time::sleep(timeout - Duration::from_millis(1)).await;
        assert!(
            !has_expired(&mut deadline).await,
            "1 ms before the deadline"
        );
        assert_eq!(deadline.silent_for(), Duration::from_millis(44_999));
        let started = tokio::time::Instant::now();
        deadline.expired().await;
        assert_eq!(
            started.elapsed(),
            Duration::from_millis(1),
            "closed at client_timeout itself, not at a later heartbeat tick"
        );
        assert_eq!(deadline.silent_for(), timeout);
        assert!(has_expired(&mut deadline).await, "stays expired");
    }

    #[tokio::test(start_paused = true)]
    async fn every_inbound_frame_moves_the_deadline() {
        let timeout = Duration::from_secs(45);
        let mut deadline = InboundDeadline::new(timeout);
        // Frames every 30 s keep it open far beyond one timeout.
        for _ in 0..8 {
            tokio::time::sleep(Duration::from_secs(30)).await;
            assert!(!has_expired(&mut deadline).await);
            deadline.touch();
            assert_eq!(deadline.silent_for(), Duration::ZERO);
        }
        // Then silence: it expires 45 s after the last frame, to the millisecond.
        let last_frame = tokio::time::Instant::now();
        deadline.expired().await;
        assert_eq!(last_frame.elapsed(), timeout);
    }

    #[tokio::test(start_paused = true)]
    async fn heartbeats_and_pings_go_out_every_interval() {
        let (task, mut out) = heartbeat();
        tokio::time::sleep(Duration::from_secs(46)).await;
        let mut sent = Vec::new();
        while let Ok(o) = out.try_recv() {
            sent.push(o);
        }
        let beats = sent
            .iter()
            .filter(|o| matches!(o, Out::Text(t) if t.contains("heartbeat")))
            .count();
        let pings = sent.iter().filter(|o| matches!(o, Out::Ping)).count();
        // Ticks at 15, 30 and 45 s.
        assert_eq!((beats, pings), (3, 3), "{sent:?}");
        task.abort();
    }

    #[tokio::test]
    async fn the_writer_neither_spins_on_a_closed_batch_queue_nor_outlives_its_connection() {
        let polls = Arc::new(AtomicUsize::new(0));
        let (hi_tx, hi_rx) = mpsc::unbounded_channel();
        let (done_tx, done_rx) = oneshot::channel();
        let writer = tokio::spawn(write_loop(
            futures::sink::drain(),
            hi_rx,
            ClosedStream(polls.clone()),
            done_rx,
        ));
        // A request still running holds `hi_tx`; the batch queue is closed.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!writer.is_finished());
        assert_eq!(
            polls.load(Ordering::SeqCst),
            1,
            "a closed batch queue is polled once"
        );
        hi_tx.send(Out::Text("late response".into())).unwrap();
        done_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), writer)
            .await
            .expect("the writer ends with its connection")
            .unwrap();
        drop(hi_tx);
    }

    /// A log whose reads fail a given number of times, then serve one event.
    struct FlakyLog {
        failures: AtomicUsize,
        corrupt: bool,
        hub: watch::Sender<u64>,
    }

    impl StreamLog for FlakyLog {
        fn subscribe_head(&self, _stream: &str) -> watch::Receiver<u64> {
            self.hub.subscribe()
        }

        fn read_batch(
            &self,
            stream: String,
            after: u64,
        ) -> impl Future<Output = CoreResult<Batch>> + Send {
            let fail = self.failures.load(Ordering::SeqCst) > 0;
            if fail {
                self.failures.fetch_sub(1, Ordering::SeqCst);
            }
            let corrupt = self.corrupt;
            async move {
                if fail {
                    return Err(if corrupt {
                        CoreError::Corrupt(format!("{stream}#1 cannot be decoded"))
                    } else {
                        CoreError::Internal("the reader connection is busy".into())
                    });
                }
                let events = if after < 1 {
                    vec![EventEnvelope {
                        seq: 1,
                        seq_from: None,
                        ts: 1,
                        event: Event::CommandsChanged {},
                    }]
                } else {
                    Vec::new()
                };
                Ok(Batch {
                    events,
                    last_seq: after.max(1),
                    head: 1,
                })
            }
        }
    }

    #[tokio::test]
    async fn a_failed_read_is_retried() {
        let log = Arc::new(FlakyLog {
            failures: AtomicUsize::new(2),
            corrupt: false,
            hub: watch::channel(1).0,
        });
        let (out_tx, mut out_rx) = mpsc::channel(4);
        let heads: Heads = Arc::default();
        heads.lock().insert("workspace".into(), 1);
        let task = tokio::spawn(tail(
            log,
            "workspace".into(),
            0,
            out_tx,
            heads,
            Duration::from_millis(20),
        ));
        let delivered = tokio::time::timeout(Duration::from_secs(5), out_rx.recv())
            .await
            .expect("the batch arrives after the retries");
        assert!(matches!(delivered, Some(Out::Text(t)) if t.contains("stream/batch")));
        task.abort();
    }

    /// A log whose events after the cursor were deleted, with a head that stays behind them
    /// (the hub still holds the head published before the deletion). Counts the reads.
    struct PrunedLog {
        head: u64,
        reads: AtomicUsize,
        hub: watch::Sender<u64>,
    }

    impl StreamLog for PrunedLog {
        fn subscribe_head(&self, _stream: &str) -> watch::Receiver<u64> {
            self.hub.subscribe()
        }

        fn read_batch(
            &self,
            _stream: String,
            after: u64,
        ) -> impl Future<Output = CoreResult<Batch>> + Send {
            self.reads.fetch_add(1, Ordering::SeqCst);
            let head = self.head;
            async move {
                Ok(Batch {
                    events: Vec::new(),
                    last_seq: after,
                    head,
                })
            }
        }
    }

    #[tokio::test]
    async fn deleted_events_move_the_client_on_without_spinning() {
        // seq 101 (a `native` event) was deleted by retention; the hub still says 101.
        let log = Arc::new(PrunedLog {
            head: 101,
            reads: AtomicUsize::new(0),
            hub: watch::channel(101).0,
        });
        let (out_tx, mut out_rx) = mpsc::channel(4);
        let heads: Heads = Arc::default();
        heads.lock().insert("workspace".into(), 101);
        let task = tokio::spawn(tail(
            log.clone(),
            "workspace".into(),
            100,
            out_tx,
            heads,
            Duration::from_millis(20),
        ));
        let Some(Out::Text(text)) = tokio::time::timeout(Duration::from_secs(5), out_rx.recv())
            .await
            .expect("a batch")
        else {
            panic!("expected a batch");
        };
        let msg: RpcMessage = serde_json::from_str(&text).unwrap();
        let batch: StreamBatch = serde_json::from_value(msg.params.unwrap()).unwrap();
        assert_eq!(
            (batch.head, batch.events.len()),
            (101, 0),
            "an empty batch moves the client to the head"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(
            log.reads.load(Ordering::SeqCst),
            1,
            "the tailer waits for a commit instead of reading again"
        );
        assert!(out_rx.try_recv().is_err(), "the client is told once");
        // The next commit is read (and there is nothing new to say).
        log.hub.send_replace(102);
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(log.reads.load(Ordering::SeqCst), 2);
        task.abort();
    }

    #[tokio::test]
    async fn a_deleted_stream_does_not_make_its_tailer_spin() {
        // The thread was removed: its stream is gone (head 0) while the hub still says 510.
        let log = Arc::new(PrunedLog {
            head: 0,
            reads: AtomicUsize::new(0),
            hub: watch::channel(510).0,
        });
        let (out_tx, mut out_rx) = mpsc::channel(4);
        let task = tokio::spawn(tail(
            log.clone(),
            "thread:thr_x".into(),
            480,
            out_tx,
            Heads::default(),
            Duration::from_millis(20),
        ));
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(log.reads.load(Ordering::SeqCst), 1);
        assert!(out_rx.try_recv().is_err(), "nothing to deliver");
        task.abort();
    }

    #[tokio::test]
    async fn undecodable_data_ends_the_subscription() {
        let log = Arc::new(FlakyLog {
            failures: AtomicUsize::new(1),
            corrupt: true,
            hub: watch::channel(1).0,
        });
        let (out_tx, _out_rx) = mpsc::channel(4);
        let task = tokio::spawn(tail(
            log,
            "workspace".into(),
            0,
            out_tx,
            Heads::default(),
            Duration::from_millis(20),
        ));
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("the tailer ends")
            .unwrap();
    }
}
