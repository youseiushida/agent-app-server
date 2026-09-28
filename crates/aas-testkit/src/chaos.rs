//! A TCP proxy that misbehaves on command: drops connections, delays traffic, or silently
//! stops forwarding (a half-open connection, as after a network switch on a phone).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio::task::AbortHandle;

/// Size of one read of a forwarded connection (the largest chunk forwarded at once). A pure
/// buffer size.
const PUMP_CHUNK_BYTES: usize = 16 * 1024;

/// How the proxy treats traffic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Forward immediately.
    Pass,
    /// Wait this long before forwarding each chunk.
    Delay(Duration),
    /// Stop forwarding in both directions without closing anything.
    Blackhole,
}

struct State {
    /// Where new connections are forwarded (changeable: a restarted server listens elsewhere).
    upstream: Mutex<SocketAddr>,
    mode: watch::Sender<Mode>,
    pumps: Mutex<Vec<AbortHandle>>,
}

pub struct ChaosProxy {
    addr: SocketAddr,
    state: Arc<State>,
    accept: AbortHandle,
}

impl ChaosProxy {
    /// Listens on an ephemeral loopback port and forwards to `upstream`.
    pub async fn start(upstream: SocketAddr) -> std::io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let state = Arc::new(State {
            upstream: Mutex::new(upstream),
            mode: watch::channel(Mode::Pass).0,
            pumps: Mutex::new(Vec::new()),
        });
        let accept_state = state.clone();
        let accept = tokio::spawn(async move {
            while let Ok((client, _)) = listener.accept().await {
                let upstream = *accept_state.upstream.lock();
                // An unreachable upstream (a server being restarted) drops the client, which
                // sees a failed connection and retries.
                let Ok(server) = TcpStream::connect(upstream).await else {
                    continue;
                };
                let _ = client.set_nodelay(true);
                let _ = server.set_nodelay(true);
                let (cr, cw) = client.into_split();
                let (sr, sw) = server.into_split();
                let a = tokio::spawn(pump(cr, sw, accept_state.mode.subscribe()));
                let b = tokio::spawn(pump(sr, cw, accept_state.mode.subscribe()));
                let mut pumps = accept_state.pumps.lock();
                pumps.retain(|h| !h.is_finished());
                pumps.push(a.abort_handle());
                pumps.push(b.abort_handle());
            }
        })
        .abort_handle();
        Ok(Self {
            addr,
            state,
            accept,
        })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn set_mode(&self, mode: Mode) {
        self.state.mode.send_replace(mode);
    }

    /// Forwards connections accepted from now on to `upstream`; the proxy keeps its own port.
    pub fn set_upstream(&self, upstream: SocketAddr) {
        *self.state.upstream.lock() = upstream;
    }

    /// Abruptly ends every current connection (both sides see the socket go away).
    pub fn drop_all(&self) {
        for h in self.state.pumps.lock().drain(..) {
            h.abort();
        }
    }
}

impl Drop for ChaosProxy {
    fn drop(&mut self) {
        self.accept.abort();
        self.drop_all();
    }
}

async fn pump(
    mut from: tokio::net::tcp::OwnedReadHalf,
    mut to: tokio::net::tcp::OwnedWriteHalf,
    mut mode: watch::Receiver<Mode>,
) {
    let mut buf = vec![0u8; PUMP_CHUNK_BYTES];
    loop {
        // While blackholed, do not even read: the peer's data piles up in kernel buffers and
        // neither side learns anything, exactly like a dead network path.
        while *mode.borrow_and_update() == Mode::Blackhole {
            if mode.changed().await.is_err() {
                return;
            }
        }
        let n = match from.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => {
                // A failed connection ends like a closed one: the other side sees it close.
                tracing::debug!(error = %e, "chaos proxy: reading a connection failed; closing it");
                break;
            }
        };
        loop {
            let current = *mode.borrow_and_update();
            match current {
                Mode::Pass => break,
                Mode::Delay(d) => {
                    tokio::time::sleep(d).await;
                    break;
                }
                Mode::Blackhole => {
                    if mode.changed().await.is_err() {
                        return;
                    }
                }
            }
        }
        if let Err(e) = to.write_all(&buf[..n]).await {
            tracing::debug!(error = %e, "chaos proxy: forwarding failed; closing the connection");
            break;
        }
    }
    // The peer may already be gone; the connection is over either way.
    let _ = to.shutdown().await;
}
