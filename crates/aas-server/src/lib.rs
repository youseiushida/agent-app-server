//! Transport of agent-app-server (design.md §7).
//!
//! * `GET /v1/ws` — WebSocket, JSON-RPC 2.0, one message per text frame, device-token auth.
//! * Each subscription is a task tailing the event log from the client's cursor; replay and
//!   live delivery share that single path.
//! * Heartbeats and WebSocket pings every `heartbeat_interval`; a connection silent for
//!   `client_timeout` is closed.
//! * Responses and heartbeats bypass stream batches (two-level send queue).
//! * One active connection per device: a new one replaces the old (`connection/replaced`).
//! * Requests touching the same thread or project are processed in arrival order; others run
//!   concurrently.
//! * The admin API (`/v1/admin/*`) and the liveness endpoint (`/v1/liveness`) are served on a
//!   separate listener that must be bound to loopback ([`Server::serve_admin`]); the public
//!   listener (the one `tailscale serve` publishes) does not route them at all.

mod admin;
mod conn;
mod errors;
mod http;
mod lanes;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use aas_core::Engine;
use aas_core::config::{MIN_TIMER, PolicyField, check_policy};
use aas_protocol::DeviceId;
pub use aas_protocol::notifications::ShutdownReason;
use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::routing::{delete, get, post};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use tokio::sync::{oneshot, watch};
use tokio_util::task::TaskTracker;

pub use admin::StopRequest;

/// Transport options.
#[derive(Debug, Clone)]
pub struct ServerOptions {
    /// Address the public listener is bound to (shown by `status`).
    pub listen: SocketAddr,
    /// WebSocket URL clients use from outside (put into pairing QR codes).
    pub public_url: Option<String>,
    /// Secret required by `/v1/admin/*`.
    pub admin_token: String,
}

/// Policy values of the transport (`[policy]` in config.toml, next to the engine's; see
/// docs/design.md §13).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerPolicy {
    /// How long a closing connection lets its writer deliver what is queued (the close frame
    /// included) before the writer is aborted. Long enough for a live client on a slow mobile
    /// link to receive `server/shuttingDown` and the close frame; short enough that a peer
    /// which stopped reading cannot keep the connection's resources (or a daemon shutdown)
    /// waiting.
    #[serde(with = "humantime_serde")]
    pub writer_flush_timeout: Duration,
    /// `stream/batch` notifications queued per connection. Tailers wait while the queue is
    /// full, so a slow client only delays its own cursors (the event log is the buffer, not
    /// memory). Four batches keep the socket busy while the next batch is read from the log,
    /// and bound what an interaction event can wait behind to four batches.
    pub stream_batch_queue: usize,
    /// Deadline of the liveness round trip (`GET /v1/liveness`: an engine database read). A
    /// read takes milliseconds; five seconds tolerate a busy disk (antivirus scans, a WAL
    /// checkpoint) while a daemon whose database access is stuck is reported as not live.
    #[serde(with = "humantime_serde")]
    pub liveness_deadline: Duration,
    /// How long closing the public listener waits for what is still in flight: HTTP responses
    /// being sent (a blob download) and WebSocket connections delivering `server/shuttingDown`
    /// and their close frame. Past it, what is left is dropped and the daemon goes on stopping.
    /// Twice `writer_flush_timeout`: every live connection gets its full flush, and a response
    /// whose client stopped reading (a phone that froze the app while its TCP connection stays
    /// open) cannot keep a stop — or a fail-stop the watchdog waits for — from finishing.
    #[serde(with = "humantime_serde")]
    pub transport_shutdown_timeout: Duration,
}

impl Default for ServerPolicy {
    fn default() -> Self {
        Self {
            writer_flush_timeout: Duration::from_secs(5),
            stream_batch_queue: 4,
            liveness_deadline: Duration::from_secs(5),
            transport_shutdown_timeout: Duration::from_secs(10),
        }
    }
}

impl ServerPolicy {
    /// Every value with its lower bound (docs/design.md §13).
    pub fn fields(&self) -> Vec<(&'static str, PolicyField)> {
        vec![
            (
                "writer_flush_timeout",
                PolicyField::duration(self.writer_flush_timeout, MIN_TIMER),
            ),
            (
                "stream_batch_queue",
                PolicyField::count(self.stream_batch_queue, 1),
            ),
            (
                "liveness_deadline",
                PolicyField::duration(self.liveness_deadline, MIN_TIMER),
            ),
            (
                "transport_shutdown_timeout",
                PolicyField::duration(self.transport_shutdown_timeout, MIN_TIMER),
            ),
        ]
    }

    /// Rejects values below their lower bound and inconsistent combinations, naming every
    /// offending key.
    pub fn validate(&self) -> Result<(), String> {
        check_policy(
            &self.fields(),
            &[(
                self.transport_shutdown_timeout >= self.writer_flush_timeout,
                "policy.transport_shutdown_timeout must not be shorter than policy.writer_flush_timeout"
                    .to_owned(),
            )],
        )
    }
}

/// What connected clients are told when the transport shuts down (`server/shuttingDown`).
/// Decided once, by whoever stops the server, so that every connection reports the same
/// reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShutdownNotice {
    pub reason: ShutdownReason,
    pub restart_expected: bool,
}

pub(crate) struct ConnEntry {
    pub id: u64,
    pub replace: oneshot::Sender<CloseReason>,
}

/// Why the server closes one connection (shutdowns reach every connection through
/// [`AppState::shutdown_rx`] instead).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CloseReason {
    Replaced,
    Revoked,
}

pub(crate) struct AppState {
    pub engine: Arc<Engine>,
    pub options: ServerOptions,
    pub policy: ServerPolicy,
    pub connections: Mutex<HashMap<DeviceId, ConnEntry>>,
    pub next_conn_id: std::sync::atomic::AtomicU64,
    pub stop_tx: watch::Sender<Option<StopRequest>>,
    /// `Some` once the transport shuts down; every connection closes with this notice.
    pub shutdown_rx: watch::Receiver<Option<ShutdownNotice>>,
    /// One token per WebSocket connection, from the upgrade request until the connection has
    /// closed. axum hands upgraded connections to tasks of their own, which its graceful
    /// shutdown does not wait for; [`Server::run_until`] waits for these instead.
    pub connection_tasks: TaskTracker,
}

/// The HTTP/WebSocket server.
pub struct Server {
    state: Arc<AppState>,
    shutdown_tx: watch::Sender<Option<ShutdownNotice>>,
}

impl Server {
    /// A server with the default [`ServerPolicy`] (see [`Server::with_policy`]).
    pub fn new(engine: Arc<Engine>, options: ServerOptions) -> Self {
        Self::with_policy(engine, options, ServerPolicy::default())
    }

    pub fn with_policy(engine: Arc<Engine>, options: ServerOptions, policy: ServerPolicy) -> Self {
        let (stop_tx, _) = watch::channel(None);
        let (shutdown_tx, shutdown_rx) = watch::channel(None);
        let state = Arc::new(AppState {
            engine,
            options,
            policy,
            connections: Mutex::new(HashMap::new()),
            next_conn_id: std::sync::atomic::AtomicU64::new(1),
            stop_tx,
            shutdown_rx,
            connection_tasks: TaskTracker::new(),
        });
        Self { state, shutdown_tx }
    }

    /// Stop requests made through the admin API (`agent-app-server stop`).
    pub fn stop_requests(&self) -> watch::Receiver<Option<StopRequest>> {
        self.state.stop_tx.subscribe()
    }

    /// Routes of the public listener (the one `tailscale serve` publishes).
    pub fn router(&self) -> Router {
        let max_blob = self.state.engine.policy().max_blob_bytes as usize;
        Router::new()
            .route("/v1/healthz", get(http::health))
            .route("/v1/pair", post(http::pair))
            .route(
                "/v1/blobs",
                post(http::upload_blob).layer(DefaultBodyLimit::max(max_blob)),
            )
            .route("/v1/blobs/{id}", get(http::download_blob))
            .route("/v1/ws", get(conn::upgrade))
            .fallback(errors::not_found)
            .method_not_allowed_fallback(errors::method_not_allowed)
            .with_state(self.state.clone())
    }

    /// Routes of the loopback-only admin listener: the admin API and the liveness endpoint.
    pub fn admin_router(&self) -> Router {
        Router::new()
            .route("/v1/liveness", get(admin::liveness))
            .route("/v1/admin/pairing-codes", post(admin::pairing_code))
            .route("/v1/admin/devices", get(admin::devices))
            .route("/v1/admin/devices/{id}", delete(admin::revoke))
            .route("/v1/admin/status", get(admin::status))
            .route(
                "/v1/admin/harnesses/refresh",
                post(admin::refresh_harnesses),
            )
            .route("/v1/admin/stop", post(admin::stop))
            .fallback(errors::not_found)
            .method_not_allowed_fallback(errors::method_not_allowed)
            .with_state(self.state.clone())
    }

    /// Serves the admin API on `listener` until `shutdown` resolves. Independent of the
    /// public listener, so `status`, `stop` and the liveness endpoint keep answering while
    /// the transport drains and closes.
    ///
    /// The listener must be bound to a loopback address: the admin API is for the local CLI
    /// and the watchdog, and a listener reachable from the network is refused.
    pub fn serve_admin(
        &self,
        listener: TcpListener,
        shutdown: impl Future<Output = ()> + Send + 'static,
    ) -> std::io::Result<impl Future<Output = std::io::Result<()>> + Send + 'static> {
        check_admin_addr(listener.local_addr()?)?;
        let app = self.admin_router();
        Ok(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(shutdown)
                .await
        })
    }

    /// Serves the public routes until `shutdown` resolves, then tells connected clients why
    /// (`server/shuttingDown` with the engine's [`shutdown_reason`](Engine::shutdown_reason)
    /// at that moment: `storageFailure`, `drain` or `shutdown`) and closes them.
    ///
    /// For a server nothing restarts by itself: `restartExpected` is `false`. A server run by
    /// a supervisor that restarts it after a failure (the daemon under its watchdog) decides
    /// the notice itself with [`run_until`](Self::run_until).
    pub async fn run(
        self,
        listener: TcpListener,
        shutdown: impl Future<Output = ()> + Send + 'static,
    ) -> std::io::Result<()> {
        let engine = self.state.engine.clone();
        self.run_until(listener, async move {
            shutdown.await;
            ShutdownNotice {
                reason: engine.shutdown_reason(),
                restart_expected: false,
            }
        })
        .await
    }

    /// Serves the public routes until `shutdown` resolves with the notice every connected
    /// client receives before its connection is closed (close code 1001).
    ///
    /// Returns once the listener is closed and every connection has ended: HTTP requests in
    /// flight have been answered and every WebSocket connection has delivered the notice and
    /// its close frame (each within `writer_flush_timeout`). What is still in flight after
    /// `transport_shutdown_timeout` is dropped, so a client that stopped reading cannot hold
    /// the daemon's stop.
    pub async fn run_until(
        self,
        listener: TcpListener,
        shutdown: impl Future<Output = ShutdownNotice> + Send + 'static,
    ) -> std::io::Result<()> {
        let app = self
            .router()
            .into_make_service_with_connect_info::<SocketAddr>();
        let state = self.state.clone();
        tokio::spawn(revocation_watch(state.clone()));
        let shutdown_tx = self.shutdown_tx;
        let (closing_tx, mut closing_rx) = oneshot::channel::<()>();
        let serving = axum::serve(listener, app).with_graceful_shutdown(async move {
            let notice = shutdown.await;
            // One notice for everyone: each connection reads it from this watch, also a
            // connection that registers after this point.
            shutdown_tx.send_replace(Some(notice));
            let _ = closing_tx.send(());
        });
        let mut serving = std::pin::pin!(serving.into_future());
        // The HTTP side may be done in the same poll that begins the shutdown (nothing was in
        // flight); the WebSocket connections are waited for in any case.
        let http_done = tokio::select! {
            ended = &mut serving => {
                if closing_rx.try_recv().is_err() {
                    // Stopped without a shutdown (the listener failed).
                    state.connection_tasks.close();
                    return ended;
                }
                Some(ended)
            }
            _ = &mut closing_rx => None,
        };
        let limit = state.policy.transport_shutdown_timeout;
        let deadline = tokio::time::Instant::now() + limit;
        state.connection_tasks.close();
        let http = async {
            match http_done {
                Some(ended) => Ok(ended),
                None => tokio::time::timeout_at(deadline, &mut serving).await,
            }
        };
        let (http, websockets) = tokio::join!(
            http,
            tokio::time::timeout_at(deadline, state.connection_tasks.wait())
        );
        if websockets.is_err() {
            tracing::warn!(open = state.connection_tasks.len(), timeout = ?limit, "WebSocket connections still open when the transport shutdown timed out; dropped");
        }
        match http {
            Ok(ended) => ended,
            Err(_) => {
                tracing::warn!(timeout = ?limit, "HTTP requests still in flight when the transport shutdown timed out; dropped");
                Ok(())
            }
        }
    }
}

/// The admin API is served only on loopback addresses (the local CLI and the watchdog are its
/// only clients).
pub fn check_admin_addr(addr: SocketAddr) -> std::io::Result<()> {
    if addr.ip().is_loopback() {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("the admin listener must be bound to a loopback address, not {addr}"),
        ))
    }
}

/// Closes connections of devices revoked while connected. Ends with the server, releasing its
/// hold on the engine.
async fn revocation_watch(state: Arc<AppState>) {
    let mut rx = state.engine.revocations();
    let mut shutdown = state.shutdown_rx.clone();
    loop {
        let received = tokio::select! {
            received = rx.recv() => received,
            // The server stopped (or its sender is gone): nothing is connected any more.
            _ = shutdown.wait_for(Option::is_some) => return,
        };
        match received {
            Ok(device) => close_connection(&state, &device, CloseReason::Revoked),
            Err(tokio::sync::broadcast::error::RecvError::Lagged(missed)) => {
                // Some revocations were not seen: check every connected device instead.
                tracing::warn!(
                    missed,
                    "revocation notices were missed; checking every connected device"
                );
                close_revoked(&state).await;
            }
            Err(_) => return,
        }
    }
}

fn close_connection(state: &AppState, device: &DeviceId, reason: CloseReason) {
    if let Some(c) = state.connections.lock().remove(device) {
        let _ = c.replace.send(reason);
    }
}

/// Closes the connection of every connected device that is no longer active.
async fn close_revoked(state: &AppState) {
    let devices: Vec<DeviceId> = state.connections.lock().keys().cloned().collect();
    for device in devices {
        match state.engine.device_active(&device).await {
            Ok(true) => {}
            Ok(false) => close_connection(state, &device, CloseReason::Revoked),
            Err(e) => tracing::warn!(device = %device, error = %e, "could not check the device"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_policy_is_valid_and_round_trips() {
        let p = ServerPolicy::default();
        p.validate().unwrap();
        let json = serde_json::to_value(&p).unwrap();
        assert_eq!(json["writer_flush_timeout"], "5s");
        assert_eq!(json["stream_batch_queue"], 4);
        assert_eq!(json["transport_shutdown_timeout"], "10s");
        let back: ServerPolicy = serde_json::from_value(json).unwrap();
        assert_eq!(back, p);
        assert!(
            ServerPolicy {
                stream_batch_queue: 0,
                ..ServerPolicy::default()
            }
            .validate()
            .is_err()
        );
        let short = ServerPolicy {
            transport_shutdown_timeout: Duration::from_secs(1),
            ..ServerPolicy::default()
        };
        assert!(
            short.validate().is_err(),
            "a live connection gets its whole flush"
        );
    }

    #[test]
    fn every_transport_value_has_a_lower_bound_that_is_enforced() {
        aas_core::config::verify_policy_bounds(ServerPolicy::fields, ServerPolicy::validate)
            .unwrap();
        let zero = ServerPolicy {
            writer_flush_timeout: Duration::ZERO,
            liveness_deadline: Duration::ZERO,
            ..ServerPolicy::default()
        };
        let err = zero.validate().unwrap_err();
        assert!(
            err.contains("policy.writer_flush_timeout must be at least")
                && err.contains("policy.liveness_deadline must be at least"),
            "{err}"
        );
    }

    /// An engine with the in-process fake harness in a temporary folder.
    async fn engine() -> (Arc<Engine>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let policy = aas_core::Policy {
            prevent_sleep_while_running: false,
            ..aas_core::Policy::default()
        };
        let supervisor = aas_supervisor::Supervisor::new(
            &data.join("supervisor"),
            aas_supervisor::SupervisorPolicy {
                prevent_sleep: false,
                ..policy.supervisor_policy()
            },
        )
        .unwrap();
        let ctx = aas_harness::AdapterContext {
            supervisor: supervisor.clone(),
            state_dir: data.join("adapters/fake"),
            policy: policy.adapter_policy(),
        };
        let registry = aas_core::HarnessRegistry::new(vec![Arc::new(
            aas_adapter_fake::FakeAdapter::in_process("fake", ctx),
        )]);
        let config = aas_core::EngineConfig {
            data_dir: data,
            server_name: "test".into(),
            hostname: "host".into(),
            project_roots: Vec::new(),
            policy,
            heuristics: Default::default(),
            git: None,
        };
        (
            Engine::start(config, registry, supervisor).await.unwrap(),
            dir,
        )
    }

    async fn serving(policy: ServerPolicy) -> (Server, TcpListener, tempfile::TempDir) {
        let (engine, dir) = engine().await;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let options = ServerOptions {
            listen: listener.local_addr().unwrap(),
            public_url: None,
            admin_token: "t".into(),
        };
        (Server::with_policy(engine, options, policy), listener, dir)
    }

    const NOTICE: ShutdownNotice = ShutdownNotice {
        reason: ShutdownReason::Shutdown,
        restart_expected: true,
    };

    #[tokio::test(flavor = "multi_thread")]
    async fn the_transport_closes_only_after_its_websocket_connections() {
        let policy = ServerPolicy {
            writer_flush_timeout: Duration::from_secs(1),
            transport_shutdown_timeout: Duration::from_secs(30),
            ..ServerPolicy::default()
        };
        let (server, listener, _dir) = serving(policy).await;
        // A connection still delivering `server/shuttingDown` and its close frame (axum's
        // graceful shutdown does not count upgraded connections).
        let connection = server.state.connection_tasks.token();
        let run = tokio::spawn(server.run_until(listener, async { NOTICE }));
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            !run.is_finished(),
            "the transport waits for its connections"
        );
        drop(connection);
        tokio::time::timeout(Duration::from_secs(10), run)
            .await
            .expect("returns once they are gone")
            .unwrap()
            .unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_connection_that_never_ends_is_dropped_at_the_shutdown_deadline() {
        let limit = Duration::from_secs(1);
        let policy = ServerPolicy {
            writer_flush_timeout: limit,
            transport_shutdown_timeout: limit,
            ..ServerPolicy::default()
        };
        let (server, listener, _dir) = serving(policy).await;
        let _stuck = server.state.connection_tasks.token();
        let started = tokio::time::Instant::now();
        tokio::time::timeout(
            Duration::from_secs(10),
            server.run_until(listener, async { NOTICE }),
        )
        .await
        .expect("the deadline ends the wait")
        .unwrap();
        assert!(started.elapsed() >= limit);
    }

    #[test]
    fn the_admin_listener_must_be_loopback() {
        assert!(check_admin_addr("127.0.0.1:7879".parse().unwrap()).is_ok());
        assert!(check_admin_addr("[::1]:7879".parse().unwrap()).is_ok());
        let err = check_admin_addr("0.0.0.0:7879".parse().unwrap()).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        assert!(check_admin_addr("100.64.0.2:7879".parse().unwrap()).is_err());
    }
}
