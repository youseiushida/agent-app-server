//! Tiny HTTP/1.1 client for the loopback admin listener (no TLS, one request per connection).
//! Used by the CLI (admin API) and by the watchdog (liveness endpoint).

use std::net::SocketAddr;
use std::time::Duration;

use anyhow::{Context, bail};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use aas_protocol::methods::{HarnessListResult, HarnessRefreshParams};

use crate::config::{Config, Paths};

/// Connect and request deadlines of the client (`policy.admin_connect_timeout` and
/// `policy.admin_request_timeout`, or the liveness values for the watchdog).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timeouts {
    pub connect: Duration,
    pub request: Duration,
}

pub struct AdminClient {
    addr: SocketAddr,
    token: Option<String>,
    timeouts: Timeouts,
}

impl AdminClient {
    pub fn new(admin_listen: SocketAddr, token: Option<String>, timeouts: Timeouts) -> Self {
        Self {
            addr: admin_listen,
            token,
            timeouts,
        }
    }

    /// The client of the daemon configured in `config`, with the admin token.
    pub fn from_config(paths: &Paths, config: &Config) -> anyhow::Result<Self> {
        let p = &config.daemon_policy;
        Ok(Self::new(
            config.server.admin_listen,
            Some(crate::config::admin_token(paths)?),
            Timeouts {
                connect: p.admin_connect_timeout,
                request: p.admin_request_timeout,
            },
        ))
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// One request; the exchange after connecting is cut at `request_timeout`.
    async fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<Vec<u8>>,
        request_timeout: Duration,
    ) -> anyhow::Result<(u16, Vec<u8>)> {
        let mut stream = tokio::time::timeout(
            self.timeouts.connect,
            tokio::net::TcpStream::connect(self.addr),
        )
        .await
        .with_context(|| {
            format!(
                "connecting to the daemon's admin listener {} timed out",
                self.addr
            )
        })?
        .with_context(|| {
            format!(
                "the daemon is not running on {} (start it with `agent-app-server run`)",
                self.addr
            )
        })?;
        if self.token.is_some() {
            // The token goes only to a listener of this user (another account may hold the
            // port while the daemon does not).
            crate::listener_owner::ensure_same_user(&stream)?;
        }
        let exchange = async {
            let body = body.unwrap_or_default();
            let mut head = format!("{method} {path} HTTP/1.1\r\nHost: {}\r\n", self.addr);
            if let Some(token) = &self.token {
                head.push_str(&format!("Authorization: Bearer {token}\r\n"));
            }
            head.push_str(&format!(
                "Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            ));
            stream.write_all(head.as_bytes()).await?;
            stream.write_all(&body).await?;
            let mut buf = Vec::new();
            stream.read_to_end(&mut buf).await?;
            anyhow::Ok(buf)
        };
        let buf = tokio::time::timeout(request_timeout, exchange)
            .await
            .map_err(|_| NoAnswer {
                method: method.to_owned(),
                path: path.to_owned(),
                after: request_timeout,
            })??;
        let split = buf
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .context("malformed HTTP response")?;
        let head = String::from_utf8_lossy(&buf[..split]).to_string();
        let status: u16 = head
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .context("malformed status line")?;
        let mut body = buf[split + 4..].to_vec();
        if head
            .to_ascii_lowercase()
            .contains("transfer-encoding: chunked")
        {
            body = dechunk(&body)?;
        }
        Ok((status, body))
    }

    pub async fn get<T: DeserializeOwned>(&self, path: &str) -> anyhow::Result<T> {
        let (status, body) = self
            .request("GET", path, None, self.timeouts.request)
            .await?;
        parse(status, &body)
    }

    pub async fn post<B: Serialize, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> anyhow::Result<T> {
        let (status, body) = self
            .request(
                "POST",
                path,
                Some(serde_json::to_vec(body)?),
                self.timeouts.request,
            )
            .await?;
        parse(status, &body)
    }

    pub async fn post_empty(&self, path: &str, body: serde_json::Value) -> anyhow::Result<()> {
        let (status, body) = self
            .request(
                "POST",
                path,
                Some(serde_json::to_vec(&body)?),
                self.timeouts.request,
            )
            .await?;
        check(status, &body)
    }

    pub async fn delete(&self, path: &str) -> anyhow::Result<()> {
        let (status, body) = self
            .request("DELETE", path, None, self.timeouts.request)
            .await?;
        check(status, &body)
    }

    /// One liveness check (`GET /v1/liveness`): `Ok` only for a 200 within the timeouts.
    pub async fn liveness(&self) -> anyhow::Result<()> {
        let (status, body) = self
            .request("GET", "/v1/liveness", None, self.timeouts.request)
            .await?;
        check(status, &body)
    }

    /// `POST /v1/admin/harnesses/refresh` (`agent-app-server harness refresh [id]`). The
    /// daemon answers once the probes have finished, which takes as long as the agent CLIs
    /// take to start and answer: the exchange is bounded by `timeout`
    /// (`policy.harness_refresh_timeout`), not by the admin request timeout.
    pub async fn refresh_harnesses(
        &self,
        params: &HarnessRefreshParams,
        timeout: Duration,
    ) -> anyhow::Result<HarnessListResult> {
        let (status, body) = self
            .request(
                "POST",
                HARNESS_REFRESH_PATH,
                Some(serde_json::to_vec(params)?),
                timeout,
            )
            .await
            .map_err(|e| {
                if e.is::<NoAnswer>() {
                    e.context(
                        "the probes did not finish within policy.harness_refresh_timeout (they \
                         go on in the daemon, and their results reach the app as \
                         harness/updated)",
                    )
                } else {
                    e
                }
            })?;
        parse(status, &body)
    }
}

/// The daemon accepted the connection but did not answer within the request's bound.
#[derive(Debug)]
struct NoAnswer {
    method: String,
    path: String,
    after: Duration,
}

impl std::fmt::Display for NoAnswer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the daemon did not answer {} {} within {:?}",
            self.method, self.path, self.after
        )
    }
}

impl std::error::Error for NoAnswer {}

/// The admin endpoint of `harness refresh`.
pub const HARNESS_REFRESH_PATH: &str = "/v1/admin/harnesses/refresh";

fn check(status: u16, body: &[u8]) -> anyhow::Result<()> {
    if (200..300).contains(&status) {
        return Ok(());
    }
    let message = serde_json::from_slice::<aas_protocol::http::HttpError>(body)
        .map(|e| format!("{} ({})", e.message, e.kind))
        .unwrap_or_else(|_| String::from_utf8_lossy(body).into_owned());
    bail!("the daemon answered {status}: {message}")
}

fn parse<T: DeserializeOwned>(status: u16, body: &[u8]) -> anyhow::Result<T> {
    check(status, body)?;
    serde_json::from_slice(body).context("decoding the daemon's answer")
}

fn dechunk(mut raw: &[u8]) -> anyhow::Result<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let line_end = raw
            .windows(2)
            .position(|w| w == b"\r\n")
            .context("malformed chunk")?;
        let size = usize::from_str_radix(std::str::from_utf8(&raw[..line_end])?.trim(), 16)?;
        raw = &raw[line_end + 2..];
        if size == 0 {
            return Ok(out);
        }
        anyhow::ensure!(raw.len() >= size + 2, "truncated chunk");
        out.extend_from_slice(&raw[..size]);
        raw = &raw[size + 2..];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_silent_daemon_does_not_hang_the_client() {
        // Accepts the connection and never answers.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hold = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            tokio::time::sleep(Duration::from_secs(30)).await;
            drop(socket);
        });
        let client = AdminClient::new(
            addr,
            None,
            Timeouts {
                connect: Duration::from_secs(5),
                request: Duration::from_millis(200),
            },
        );
        let started = std::time::Instant::now();
        let err = client.liveness().await.unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(5), "{err:#}");
        assert!(format!("{err:#}").contains("did not answer"), "{err:#}");
        hold.abort();
    }

    /// A daemon that answers every request after `delay` with `{"harnesses":[]}`.
    async fn slow_daemon(delay: Duration) -> (SocketAddr, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                tokio::spawn(async move {
                    // Read the whole request so that closing the socket does not reset it.
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 1024];
                    loop {
                        let n = socket.read(&mut chunk).await.unwrap();
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                        let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
                            continue;
                        };
                        let head = String::from_utf8_lossy(&buf[..end]).to_ascii_lowercase();
                        let length: usize = head
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:"))
                            .map_or(0, |v| v.trim().parse().unwrap());
                        if buf.len() >= end + 4 + length {
                            break;
                        }
                    }
                    tokio::time::sleep(delay).await;
                    let body = br#"{"harnesses":[]}"#;
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    // The client may have given up already.
                    if socket.write_all(head.as_bytes()).await.is_ok() {
                        let _ = socket.write_all(body).await;
                    }
                    let _ = socket.shutdown().await;
                });
            }
        });
        (addr, task)
    }

    #[tokio::test]
    async fn harness_refresh_waits_for_the_probes_beyond_the_admin_request_timeout() {
        let (addr, daemon) = slow_daemon(Duration::from_millis(600)).await;
        let client = AdminClient::new(
            addr,
            None,
            Timeouts {
                connect: Duration::from_secs(5),
                request: Duration::from_millis(150),
            },
        );
        let params = HarnessRefreshParams { harness_id: None };
        // Any other admin request is cut at the admin request timeout.
        let err = client
            .post::<_, HarnessListResult>(HARNESS_REFRESH_PATH, &params)
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("did not answer"), "{err:#}");
        // The refresh has its own bound, and gets the answer of the slow probes.
        let result = client
            .refresh_harnesses(&params, Duration::from_secs(20))
            .await
            .unwrap();
        assert!(result.harnesses.is_empty());
        // When that bound passes too, the error says what happens to the probes.
        let err = client
            .refresh_harnesses(&params, Duration::from_millis(150))
            .await
            .unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("policy.harness_refresh_timeout"), "{text}");
        assert!(text.contains("did not answer"), "{text}");
        // A daemon that is not running is reported as such, without the probe hint.
        daemon.abort();
        let closed = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let gone = closed.local_addr().unwrap();
        drop(closed);
        let err = AdminClient::new(
            gone,
            None,
            Timeouts {
                connect: Duration::from_secs(5),
                request: Duration::from_millis(150),
            },
        )
        .refresh_harnesses(&params, Duration::from_secs(20))
        .await
        .unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("not running"), "{text}");
        assert!(!text.contains("harness_refresh_timeout"), "{text}");
    }

    #[test]
    fn errors_carry_the_protocol_message_and_kind() {
        let err = check(
            401,
            br#"{"kind":"unauthorized","message":"invalid admin token"}"#,
        )
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "the daemon answered 401: invalid admin token (unauthorized)"
        );
        assert_eq!(dechunk(b"3\r\nabc\r\n0\r\n\r\n").unwrap(), b"abc");
        assert!(dechunk(b"9\r\nabc\r\n").is_err());
    }
}
