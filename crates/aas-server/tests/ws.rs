//! End-to-end transport tests over real TCP with the in-process fake agent.

use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use aas_adapter_fake::FakeAdapter;
use aas_core::{Engine, EngineConfig, HarnessRegistry, Policy};
use aas_harness::AdapterContext;
use aas_protocol::events::{Event, EventEnvelope};
use aas_protocol::rpc::RpcMessage;
use aas_protocol::*;
use aas_server::{Server, ServerOptions, ServerPolicy};
use aas_supervisor::{Supervisor, SupervisorPolicy};
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

const ADMIN: &str = "admin-secret-for-tests";

struct Env {
    _dir: tempfile::TempDir,
    root: std::path::PathBuf,
    /// Public listener.
    addr: SocketAddr,
    /// Admin listener (admin API and liveness).
    admin: SocketAddr,
    engine: Arc<Engine>,
    stop_requests: tokio::sync::watch::Receiver<Option<aas_server::StopRequest>>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    serving: Option<tokio::task::JoinHandle<std::io::Result<()>>>,
    _admin_stop: tokio::sync::oneshot::Sender<()>,
}

impl Env {
    /// Stops the public listener like the daemon does and waits until every connection is
    /// closed.
    async fn stop_server(&mut self) {
        let _ = self.stop.take().expect("the server runs").send(());
        tokio::time::timeout(
            Duration::from_secs(20),
            self.serving.take().expect("the server runs"),
        )
        .await
        .expect("the server stops")
        .unwrap()
        .unwrap();
    }
}

async fn start(f: impl FnOnce(&mut Policy)) -> Env {
    start_with(f, ServerPolicy::default()).await
}

async fn start_with(f: impl FnOnce(&mut Policy), server_policy: ServerPolicy) -> Env {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let root = dir.path().join("projects");
    std::fs::create_dir_all(root.join("p")).unwrap();
    let root = dunce::canonicalize(&root).unwrap();
    let mut policy = Policy {
        stop_grace: Duration::from_millis(500),
        prevent_sleep_while_running: false,
        ..Policy::default()
    };
    f(&mut policy);
    let supervisor = Supervisor::new(
        &data.join("supervisor"),
        SupervisorPolicy {
            prevent_sleep: false,
            ..policy.supervisor_policy()
        },
    )
    .unwrap();
    let ctx = AdapterContext {
        supervisor: supervisor.clone(),
        state_dir: data.join("adapters/fake"),
        policy: policy.adapter_policy(),
    };
    let registry = HarnessRegistry::new(vec![Arc::new(FakeAdapter::in_process("fake", ctx))]);
    let config = EngineConfig {
        data_dir: data,
        server_name: "test-pc".into(),
        hostname: "host".into(),
        project_roots: vec![root.clone()],
        policy,
        heuristics: Default::default(),
        git: None,
    };
    let engine = Engine::start(config, registry, supervisor).await.unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let admin_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let admin = admin_listener.local_addr().unwrap();
    let server = Server::with_policy(
        engine.clone(),
        ServerOptions {
            listen: addr,
            public_url: Some(format!("ws://{addr}/v1/ws")),
            admin_token: ADMIN.into(),
        },
        server_policy,
    );
    let stop_requests = server.stop_requests();
    let (admin_stop_tx, admin_stop_rx) = tokio::sync::oneshot::channel::<()>();
    let admin_serving = server
        .serve_admin(admin_listener, async move {
            let _ = admin_stop_rx.await;
        })
        .unwrap();
    tokio::spawn(admin_serving);
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let serving = tokio::spawn(server.run(listener, async move {
        let _ = stop_rx.await;
    }));
    Env {
        _dir: dir,
        root,
        addr,
        admin,
        engine,
        stop_requests,
        stop: Some(stop_tx),
        serving: Some(serving),
        _admin_stop: admin_stop_tx,
    }
}

/// `kind` of an error body (every HTTP error is `{kind, message}`).
fn error_kind(body: &[u8]) -> String {
    let v: Value = serde_json::from_slice(body).unwrap_or_else(|e| {
        panic!(
            "not a JSON error body ({e}): {}",
            String::from_utf8_lossy(body)
        )
    });
    assert!(v["message"].is_string(), "{v}");
    v["kind"]
        .as_str()
        .unwrap_or_else(|| panic!("no kind: {v}"))
        .to_owned()
}

/// Minimal HTTP/1.1 client (one request per connection).
async fn http(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> (u16, Vec<u8>) {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let mut req = format!(
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\nContent-Length: {}\r\n",
        body.len()
    );
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    stream.write_all(req.as_bytes()).await.unwrap();
    stream.write_all(body).await.unwrap();
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.unwrap();
    let split = buf.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let head = String::from_utf8_lossy(&buf[..split]).to_string();
    let status: u16 = head.split_whitespace().nth(1).unwrap().parse().unwrap();
    let mut body = buf[split + 4..].to_vec();
    if head
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked")
    {
        body = dechunk(&body);
    }
    (status, body)
}

fn dechunk(mut raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let line_end = raw.windows(2).position(|w| w == b"\r\n").unwrap();
        let size = usize::from_str_radix(std::str::from_utf8(&raw[..line_end]).unwrap().trim(), 16)
            .unwrap();
        raw = &raw[line_end + 2..];
        if size == 0 {
            return out;
        }
        out.extend_from_slice(&raw[..size]);
        raw = &raw[size + 2..];
    }
}

async fn pair(env: &Env, name: &str) -> String {
    let (code, _) = env.engine.create_pairing_code().await.unwrap();
    let body =
        serde_json::to_vec(&json!({"code": code, "deviceName": name, "platform": "test"})).unwrap();
    let (status, resp) = http(
        env.addr,
        "POST",
        "/v1/pair",
        &[("Content-Type", "application/json")],
        &body,
    )
    .await;
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&resp));
    let v: Value = serde_json::from_slice(&resp).unwrap();
    v["token"].as_str().unwrap().to_owned()
}

type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>;

struct Client {
    ws: Ws,
    next_id: i64,
    notifications: Vec<RpcMessage>,
    closed: Option<u16>,
}

async fn connect(env: &Env, token: &str) -> Result<Client, tokio_tungstenite::tungstenite::Error> {
    let mut req = format!("ws://{}/v1/ws", env.addr)
        .into_client_request()
        .unwrap();
    req.headers_mut()
        .insert("Authorization", format!("Bearer {token}").parse().unwrap());
    let (ws, _) = tokio_tungstenite::connect_async(req).await?;
    Ok(Client {
        ws,
        next_id: 1,
        notifications: Vec::new(),
        closed: None,
    })
}

impl Client {
    async fn send_request(&mut self, method: &str, params: Value) -> i64 {
        let id = self.next_id;
        self.next_id += 1;
        let msg = RpcMessage::request(RequestId::Number(id), method, params);
        self.ws
            .send(Message::text(serde_json::to_string(&msg).unwrap()))
            .await
            .unwrap();
        id
    }

    async fn next_message(&mut self) -> Option<RpcMessage> {
        loop {
            match tokio::time::timeout(Duration::from_secs(20), self.ws.next())
                .await
                .expect("timed out waiting for a frame")
            {
                Some(Ok(Message::Text(t))) => return Some(serde_json::from_str(&t).unwrap()),
                Some(Ok(Message::Close(frame))) => {
                    self.closed = frame.map(|f| u16::from(f.code));
                    return None;
                }
                Some(Ok(_)) => continue,
                Some(Err(_)) | None => return None,
            }
        }
    }

    async fn call(&mut self, method: &str, params: Value) -> Result<Value, RpcError> {
        let id = self.send_request(method, params).await;
        loop {
            let msg = self
                .next_message()
                .await
                .expect("connection closed during call");
            if msg.id == Some(RequestId::Number(id)) && msg.method.is_none() {
                return match (msg.result, msg.error) {
                    (Some(r), _) => Ok(r),
                    (_, Some(e)) => Err(e),
                    _ => panic!("empty response"),
                };
            }
            self.notifications.push(msg);
        }
    }

    async fn init(&mut self, epoch: Option<&str>) -> Value {
        self.call(
            "initialize",
            json!({"protocolVersion": 1, "client": {"name": "test", "version": "1", "platform": "test"}, "lastKnownEpoch": epoch}),
        )
        .await
        .unwrap()
    }

    /// Collects stream events of `stream` until `pred` matches.
    async fn events_until(
        &mut self,
        stream: &str,
        pred: impl Fn(&Event) -> bool,
    ) -> Vec<EventEnvelope> {
        let mut out = Vec::new();
        let mut pending: Vec<RpcMessage> = std::mem::take(&mut self.notifications);
        loop {
            let msg = if pending.is_empty() {
                self.next_message().await.expect("closed")
            } else {
                pending.remove(0)
            };
            if msg.method.as_deref() == Some("stream/batch") {
                let params = msg.params.unwrap();
                if params["stream"] == stream {
                    let events: Vec<EventEnvelope> =
                        serde_json::from_value(params["events"].clone()).unwrap();
                    let mut done = false;
                    for e in events {
                        done |= pred(&e.event);
                        out.push(e);
                    }
                    if done {
                        return out;
                    }
                }
            }
        }
    }
}

async fn setup_thread(c: &mut Client, env: &Env) -> (String, String) {
    let project = c
        .call(
            "project/open",
            json!({"clientRequestId": "po", "path": env.root.join("p").display().to_string()}),
        )
        .await
        .unwrap();
    let pid = project["project"]["id"].as_str().unwrap().to_owned();
    let created = c
        .call(
            "thread/create",
            json!({"clientRequestId": "tc", "projectId": pid, "harnessId": "fake"}),
        )
        .await
        .unwrap();
    (pid, created["thread"]["id"].as_str().unwrap().to_owned())
}

#[tokio::test(flavor = "multi_thread")]
async fn auth_and_initialize_are_enforced() {
    let env = start(|_| {}).await;
    assert!(
        connect(&env, "not-a-token").await.is_err(),
        "bad tokens are rejected before upgrade"
    );
    let token = pair(&env, "phone").await;
    let mut c = connect(&env, &token).await.unwrap();
    let err = c.call("harness/list", json!({})).await.unwrap_err();
    assert_eq!(err.kind(), Some(ErrorKind::NotInitialized));
    let init = c.init(None).await;
    assert_eq!(init["protocolVersion"], 1);
    assert_eq!(init["epochChanged"], false);
    assert_eq!(init["server"]["name"], "test-pc");
    let init = c.init(Some("some-other-epoch")).await;
    assert_eq!(init["epochChanged"], true);
    let bad = c
        .call("initialize", json!({"protocolVersion": 99, "client": {"name": "t", "version": "1", "platform": "t"}}))
        .await
        .unwrap_err();
    assert_eq!(bad.kind(), Some(ErrorKind::ProtocolVersionUnsupported));
    let unknown = c.call("no/such", json!({})).await.unwrap_err();
    assert_eq!(unknown.kind(), Some(ErrorKind::MethodNotFound));
}

#[tokio::test(flavor = "multi_thread")]
async fn streams_replay_after_reconnect_without_gaps_or_duplicates() {
    let env = start(|_| {}).await;
    let token = pair(&env, "phone").await;
    let mut c = connect(&env, &token).await.unwrap();
    c.init(None).await;
    let (_, thread) = setup_thread(&mut c, &env).await;
    let stream = format!("thread:{thread}");
    c.call(
        "subscribe",
        json!({"subscriptions": [{"stream": stream, "after": 0}]}),
    )
    .await
    .unwrap();
    c.call("turn/start", json!({"clientRequestId": "t1", "threadId": thread, "input": [{"type": "text", "text": "@stream 200"}]}))
        .await
        .unwrap();
    let first = c
        .events_until(&stream, |e| matches!(e, Event::TurnCompleted { .. }))
        .await;
    let cursor = first.last().unwrap().seq;
    drop(c);

    // While offline, another turn happens.
    let mut other = connect(&env, &token).await.unwrap();
    other.init(None).await;
    other
        .call("turn/start", json!({"clientRequestId": "t2", "threadId": thread, "input": [{"type": "text", "text": "@stream 300"}]}))
        .await
        .unwrap();
    drop(other);
    tokio::time::sleep(Duration::from_millis(300)).await;

    let mut c = connect(&env, &token).await.unwrap();
    c.init(Some(env.engine.epoch())).await;
    c.call(
        "subscribe",
        json!({"subscriptions": [{"stream": stream, "after": cursor}]}),
    )
    .await
    .unwrap();
    let second = c
        .events_until(
            &stream,
            |e| matches!(e, Event::TurnCompleted { turn } if turn.index == 1),
        )
        .await;
    assert!(
        second.iter().all(|e| e.seq > cursor),
        "no replay of already-applied events"
    );

    // Combined, the client saw every stored event exactly once, in order.
    let full = env.engine.read_batch(stream.clone(), 0).await.unwrap();
    let mut seen: Vec<u64> = Vec::new();
    for e in first.iter().chain(second.iter()) {
        let from = e.seq_from.unwrap_or(e.seq);
        seen.extend(from..=e.seq);
    }
    let mut sorted = seen.clone();
    sorted.sort_unstable();
    assert_eq!(seen, sorted, "events arrive in order");
    let unique: BTreeSet<u64> = seen.iter().copied().collect();
    assert_eq!(unique.len(), seen.len(), "no duplicates");
    let stored_head = full.head;
    assert!(unique.contains(&stored_head));
    // The streamed text of turn 2 equals what the engine stored.
    let text: String = second
        .iter()
        .filter_map(|e| match &e.event {
            Event::ItemDelta { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        text.starts_with("tok0 tok1"),
        "{}",
        &text[..20.min(text.len())]
    );
    assert!(text.contains("tok299 "));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_new_connection_replaces_the_old_one() {
    let env = start(|_| {}).await;
    let token = pair(&env, "phone").await;
    let mut first = connect(&env, &token).await.unwrap();
    first.init(None).await;
    let mut second = connect(&env, &token).await.unwrap();
    second.init(None).await;
    let mut saw_replaced = false;
    while let Some(msg) = first.next_message().await {
        saw_replaced |= msg.method.as_deref() == Some("connection/replaced");
    }
    assert!(saw_replaced);
    assert_eq!(first.closed, Some(4000));
    // The new connection keeps working.
    assert!(second.call("harness/list", json!({})).await.is_ok());
}

#[tokio::test(flavor = "multi_thread")]
async fn heartbeats_flow_and_silent_clients_are_closed() {
    let env = start(|p| {
        p.heartbeat_interval = Duration::from_millis(150);
        p.client_timeout = Duration::from_millis(600);
    })
    .await;
    let token = pair(&env, "phone").await;
    let mut c = connect(&env, &token).await.unwrap();
    c.init(None).await;
    c.call(
        "subscribe",
        json!({"subscriptions": [{"stream": "workspace", "after": 0}]}),
    )
    .await
    .unwrap();
    // Reading keeps the connection alive (pongs are answered while polling).
    let mut beats = 0;
    while beats < 3 {
        let msg = c.next_message().await.expect("open");
        if msg.method.as_deref() == Some("heartbeat") {
            beats += 1;
            assert!(msg.params.unwrap()["heads"].get("workspace").is_some());
        }
    }
    // Stop reading entirely: no pongs, no frames → the server closes the connection.
    // (On Windows the client may lose the close frame to a TCP reset, so the server side is
    // checked through the admin API.)
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let admin = format!("Bearer {ADMIN}");
    let (status, body) = http(
        env.admin,
        "GET",
        "/v1/admin/status",
        &[("Authorization", &admin)],
        b"",
    )
    .await;
    assert_eq!(status, 200);
    let st: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        st["connectedDevices"], 0,
        "the silent connection was dropped"
    );
    while c.next_message().await.is_some() {}
    assert!(
        c.closed.is_none() || c.closed == Some(4002),
        "unexpected close code {:?}",
        c.closed
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn resends_on_a_new_connection_are_not_executed_twice() {
    let env = start(|_| {}).await;
    let token = pair(&env, "phone").await;
    let mut c = connect(&env, &token).await.unwrap();
    c.init(None).await;
    let (_, thread) = setup_thread(&mut c, &env).await;
    let params = json!({"clientRequestId": "retry-me", "threadId": thread, "input": [{"type": "text", "text": "@sleep 300"}]});
    // Send and drop the connection without reading the response.
    c.send_request("turn/start", params.clone()).await;
    drop(c);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut c = connect(&env, &token).await.unwrap();
    c.init(None).await;
    let a = c.call("turn/start", params.clone()).await.unwrap();
    let b = c.call("turn/start", params).await.unwrap();
    assert_eq!(a, b);
    assert_eq!(a["disposition"], "started");
    let read = c
        .call("thread/read", json!({"threadId": thread}))
        .await
        .unwrap();
    assert_eq!(read["turns"].as_array().unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn blobs_admin_and_revocation() {
    let env = start(|_| {}).await;
    let token = pair(&env, "phone").await;
    let auth = format!("Bearer {token}");
    let png = b"\x89PNG\r\n\x1a\nfake".to_vec();
    let (status, body) = http(
        env.addr,
        "POST",
        "/v1/blobs",
        &[("Authorization", &auth), ("Content-Type", "image/png")],
        &png,
    )
    .await;
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    let blob: Value = serde_json::from_slice(&body).unwrap();
    let id = blob["blobId"].as_str().unwrap();
    let (status, got) = http(
        env.addr,
        "GET",
        &format!("/v1/blobs/{id}"),
        &[("Authorization", &auth)],
        b"",
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(got, png);
    let (status, body) = http(env.addr, "GET", &format!("/v1/blobs/{id}"), &[], b"").await;
    assert_eq!(status, 401);
    assert_eq!(error_kind(&body), "unauthorized");
    let (status, body) = http(
        env.addr,
        "POST",
        "/v1/blobs",
        &[("Authorization", &auth), ("Content-Type", "text/plain")],
        b"x",
    )
    .await;
    assert_eq!(status, 415);
    assert_eq!(error_kind(&body), "invalidParams");

    let admin = format!("Bearer {ADMIN}");
    let (status, body) = http(
        env.admin,
        "POST",
        "/v1/admin/pairing-codes",
        &[("Authorization", &admin)],
        b"",
    )
    .await;
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    let code: Value = serde_json::from_slice(&body).unwrap();
    assert!(
        code["pairUrl"]
            .as_str()
            .unwrap()
            .starts_with("aas://pair?u=ws%3A%2F%2F")
    );
    let (status, body) = http(
        env.admin,
        "GET",
        "/v1/admin/status",
        &[("Authorization", "Bearer wrong")],
        b"",
    )
    .await;
    assert_eq!(status, 401);
    assert_eq!(error_kind(&body), "unauthorized");
    // The public listener (the one `tailscale serve` relays to) has no admin routes at all,
    // whatever the request carries.
    let (status, body) = http(
        env.addr,
        "GET",
        "/v1/admin/status",
        &[("Authorization", &admin)],
        b"",
    )
    .await;
    assert_eq!(
        status, 404,
        "the admin API is not routed on the public listener"
    );
    assert_eq!(error_kind(&body), "notFound");
    let (status, _) = http(
        env.addr,
        "POST",
        "/v1/admin/stop",
        &[
            ("Authorization", &admin),
            ("Content-Type", "application/json"),
        ],
        br#"{"drain":false}"#,
    )
    .await;
    assert_eq!(status, 404);

    // Revoking a connected device closes its connection.
    let mut c = connect(&env, &token).await.unwrap();
    c.init(None).await;
    let devices: Value = {
        let (status, body) = http(
            env.admin,
            "GET",
            "/v1/admin/devices",
            &[("Authorization", &admin)],
            b"",
        )
        .await;
        assert_eq!(status, 200);
        serde_json::from_slice(&body).unwrap()
    };
    let device_id = devices["devices"][0]["id"].as_str().unwrap().to_owned();
    let (status, _) = http(
        env.admin,
        "DELETE",
        &format!("/v1/admin/devices/{device_id}"),
        &[("Authorization", &admin)],
        b"",
    )
    .await;
    assert_eq!(status, 204);
    while c.next_message().await.is_some() {}
    assert_eq!(c.closed, Some(4001));
    assert!(connect(&env, &token).await.is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn oversized_requests_get_a_definitive_error_and_huge_frames_close_with_1009() {
    let env = start(|p| {
        p.max_client_frame_bytes = 1024;
        p.max_transport_frame_bytes = 8192;
    })
    .await;
    let token = pair(&env, "phone").await;
    let mut c = connect(&env, &token).await.unwrap();
    c.init(None).await;
    let (_, thread) = setup_thread(&mut c, &env).await;
    // Above the protocol limit: answered with payloadTooLarge (the client drops it from its
    // outbox), and the connection stays usable.
    let big = "x".repeat(2000);
    let err = c
        .call("turn/start", json!({"clientRequestId": "big", "threadId": thread, "input": [{"type": "text", "text": big}]}))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), Some(ErrorKind::PayloadTooLarge));
    assert!(c.call("harness/list", json!({})).await.is_ok());
    // Above the transport limit: the connection is closed with "message too big".
    let huge = "y".repeat(20_000);
    c.send_request("turn/start", json!({"clientRequestId": "huge", "threadId": thread, "input": [{"type": "text", "text": huge}]}))
        .await;
    while c.next_message().await.is_some() {}
    assert_eq!(c.closed, Some(1009));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cursor_ahead_of_the_head_starts_at_the_head_without_disturbing_others() {
    let env = start(|_| {}).await;
    let token_a = pair(&env, "phone").await;
    let token_b = pair(&env, "tablet").await;
    let mut a = connect(&env, &token_a).await.unwrap();
    a.init(None).await;
    let mut b = connect(&env, &token_b).await.unwrap();
    b.init(None).await;
    let head = env
        .engine
        .stream_head(WORKSPACE_STREAM)
        .await
        .unwrap()
        .unwrap();
    let sub = b
        .call(
            "subscribe",
            json!({"subscriptions": [{"stream": "workspace", "after": head}]}),
        )
        .await
        .unwrap();
    assert_eq!(sub["subscriptions"][0]["head"], head);
    // Device A claims a cursor far beyond the head (e.g. data restored from a backup).
    let sub = a
        .call(
            "subscribe",
            json!({"subscriptions": [{"stream": "workspace", "after": head + 1_000_000}]}),
        )
        .await
        .unwrap();
    assert_eq!(
        sub["subscriptions"][0]["head"], head,
        "the answer reports the real head"
    );
    // New workspace events reach both: A was moved to the head, B was not disturbed.
    b.call(
        "project/open",
        json!({"clientRequestId": "po", "path": env.root.join("p").display().to_string()}),
    )
    .await
    .unwrap();
    let got_b = b
        .events_until("workspace", |e| matches!(e, Event::ProjectUpserted { .. }))
        .await;
    assert!(got_b.iter().all(|e| e.seq > head));
    let got_a = a
        .events_until("workspace", |e| matches!(e, Event::ProjectUpserted { .. }))
        .await;
    assert!(got_a.iter().all(|e| e.seq > head));
}

#[tokio::test(flavor = "multi_thread")]
async fn every_stop_request_is_delivered_so_a_drain_can_be_escalated() {
    let mut env = start(|_| {}).await;
    let admin = format!("Bearer {ADMIN}");
    let (status, _) = http(
        env.admin,
        "POST",
        "/v1/admin/stop",
        &[
            ("Authorization", &admin),
            ("Content-Type", "application/json"),
        ],
        br#"{"drain":true}"#,
    )
    .await;
    assert_eq!(status, 202);
    env.stop_requests.changed().await.unwrap();
    assert_eq!(
        *env.stop_requests.borrow_and_update(),
        Some(aas_server::StopRequest { drain: true })
    );
    let (status, _) = http(
        env.admin,
        "POST",
        "/v1/admin/stop",
        &[
            ("Authorization", &admin),
            ("Content-Type", "application/json"),
        ],
        br#"{"drain":false}"#,
    )
    .await;
    assert_eq!(status, 202);
    env.stop_requests.changed().await.unwrap();
    assert_eq!(
        *env.stop_requests.borrow_and_update(),
        Some(aas_server::StopRequest { drain: false })
    );
}

/// Sends raw bytes as one HTTP/1.1 request and reads the answer (status, body).
async fn http_raw(addr: SocketAddr, request: &[u8]) -> (u16, Vec<u8>) {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream.write_all(request).await.unwrap();
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.unwrap();
    let split = buf.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let head = String::from_utf8_lossy(&buf[..split]).to_string();
    let status: u16 = head.split_whitespace().nth(1).unwrap().parse().unwrap();
    let mut body = buf[split + 4..].to_vec();
    if head
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked")
    {
        body = dechunk(&body);
    }
    (status, body)
}

#[tokio::test(flavor = "multi_thread")]
async fn every_http_error_has_the_protocol_body_including_axum_rejections() {
    let env = start(|p| p.max_blob_bytes = 1024).await;
    let token = pair(&env, "phone").await;
    let auth = format!("Bearer {token}");
    let big = vec![0x89u8; 4096];

    // Above max_blob_bytes with a Content-Length: refused before the body is read.
    let (status, body) = http(
        env.addr,
        "POST",
        "/v1/blobs",
        &[("Authorization", &auth), ("Content-Type", "image/png")],
        &big,
    )
    .await;
    assert_eq!(status, 413);
    assert_eq!(error_kind(&body), "payloadTooLarge");
    // A client that waits for `100 Continue` never has to send the body.
    let head = format!(
        "POST /v1/blobs HTTP/1.1\r\nHost: x\r\nConnection: close\r\nAuthorization: {auth}\r\nContent-Type: image/png\r\nContent-Length: 4096\r\nExpect: 100-continue\r\n\r\n"
    );
    let (status, body) = http_raw(env.addr, head.as_bytes()).await;
    assert_eq!(status, 413);
    assert_eq!(error_kind(&body), "payloadTooLarge");
    // Without a length (chunked), the body limit cuts the upload off: same answer.
    let mut chunked = format!(
        "POST /v1/blobs HTTP/1.1\r\nHost: x\r\nConnection: close\r\nAuthorization: {auth}\r\nContent-Type: image/png\r\nTransfer-Encoding: chunked\r\n\r\n"
    )
    .into_bytes();
    for part in big.chunks(512) {
        chunked.extend_from_slice(format!("{:x}\r\n", part.len()).as_bytes());
        chunked.extend_from_slice(part);
        chunked.extend_from_slice(b"\r\n");
    }
    chunked.extend_from_slice(b"0\r\n\r\n");
    let (status, body) = http_raw(env.addr, &chunked).await;
    assert_eq!(status, 413);
    assert_eq!(error_kind(&body), "payloadTooLarge");
    // Within the limit it still works.
    let (status, _) = http(
        env.addr,
        "POST",
        "/v1/blobs",
        &[("Authorization", &auth), ("Content-Type", "image/png")],
        &big[..512],
    )
    .await;
    assert_eq!(status, 200);

    // Malformed JSON, JSON of the wrong shape, and a missing content type on /v1/pair.
    let json = [("Content-Type", "application/json")];
    let (status, body) = http(env.addr, "POST", "/v1/pair", &json, b"{not json").await;
    assert_eq!(status, 400);
    assert_eq!(error_kind(&body), "invalidParams");
    let (status, body) = http(
        env.addr,
        "POST",
        "/v1/pair",
        &json,
        br#"{"code":"ABCD-EFGH"}"#,
    )
    .await;
    assert_eq!(status, 400);
    assert_eq!(error_kind(&body), "invalidParams");
    let (status, body) = http(
        env.addr,
        "POST",
        "/v1/pair",
        &[("Content-Type", "text/plain")],
        br#"{"code":"x","deviceName":"y","platform":"z"}"#,
    )
    .await;
    assert_eq!(status, 415);
    assert_eq!(error_kind(&body), "invalidParams");

    // Unknown paths and methods, and a plain GET on the WebSocket endpoint.
    let (status, body) = http(env.addr, "GET", "/v1/nope", &[], b"").await;
    assert_eq!(status, 404);
    assert_eq!(error_kind(&body), "notFound");
    let (status, body) = http(env.addr, "GET", "/v1/pair", &[], b"").await;
    assert_eq!(status, 405);
    assert_eq!(error_kind(&body), "invalidRequest");
    let (status, body) = http(env.addr, "GET", "/v1/ws", &[("Authorization", &auth)], b"").await;
    assert!((400..500).contains(&status), "{status}");
    assert_eq!(error_kind(&body), "invalidRequest");

    // The admin listener maps its rejections the same way.
    let admin = format!("Bearer {ADMIN}");
    let (status, body) = http(
        env.admin,
        "POST",
        "/v1/admin/stop",
        &[
            ("Authorization", &admin),
            ("Content-Type", "application/json"),
        ],
        b"{drain",
    )
    .await;
    assert_eq!(status, 400);
    assert_eq!(error_kind(&body), "invalidParams");
    let (status, body) = http(
        env.admin,
        "GET",
        "/v1/admin/nope",
        &[("Authorization", &admin)],
        b"",
    )
    .await;
    assert_eq!(status, 404);
    assert_eq!(error_kind(&body), "notFound");
    let (status, body) = http(
        env.admin,
        "GET",
        "/v1/admin/stop",
        &[("Authorization", &admin)],
        b"",
    )
    .await;
    assert_eq!(status, 405);
    assert_eq!(error_kind(&body), "invalidRequest");
}

#[tokio::test(flavor = "multi_thread")]
async fn pairing_attempts_are_rate_limited() {
    let env = start(|p| p.pairing_attempts_per_window = 3).await;
    let json = [("Content-Type", "application/json")];
    for _ in 0..3 {
        let (status, body) = http(
            env.addr,
            "POST",
            "/v1/pair",
            &json,
            br#"{"code":"WRNG-CODE","deviceName":"x","platform":"t"}"#,
        )
        .await;
        assert_eq!(status, 400);
        assert_eq!(error_kind(&body), "invalidCode");
    }
    // The fourth attempt within the minute is refused even with a valid code.
    let (code, _) = env.engine.create_pairing_code().await.unwrap();
    let body =
        serde_json::to_vec(&json!({"code": code, "deviceName": "phone", "platform": "test"}))
            .unwrap();
    let (status, resp) = http(env.addr, "POST", "/v1/pair", &json, &body).await;
    assert_eq!(status, 429);
    assert_eq!(error_kind(&resp), "rateLimited");
    assert!(
        env.engine.list_devices(None).await.unwrap().is_empty(),
        "no device was paired"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_liveness_endpoint_round_trips_through_the_engine() {
    let env = start(|_| {}).await;
    let (status, body) = http(env.admin, "GET", "/v1/liveness", &[], b"").await;
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    assert_eq!(
        serde_json::from_slice::<Value>(&body).unwrap(),
        json!({"ok": true})
    );
    let (status, _) = http(env.addr, "GET", "/v1/liveness", &[], b"").await;
    assert_eq!(status, 404, "liveness is only on the admin listener");
    // An engine that cannot read its database is not live.
    env.engine.shutdown(false).await;
    env.engine.close().await.unwrap();
    let (status, body) = http(env.admin, "GET", "/v1/liveness", &[], b"").await;
    assert_eq!(status, 503);
    assert_eq!(error_kind(&body), "unavailable");
}

/// Connects `n` initialized clients.
async fn clients(env: &Env, n: usize) -> Vec<Client> {
    let mut out = Vec::new();
    for i in 0..n {
        let token = pair(env, &format!("device-{i}")).await;
        let mut c = connect(env, &token).await.unwrap();
        c.init(None).await;
        out.push(c);
    }
    out
}

/// The `server/shuttingDown` a client received before its connection closed (with 1001).
async fn shutdown_notice(c: &mut Client) -> Value {
    let mut notice = None;
    while let Some(msg) = c.next_message().await {
        if msg.method.as_deref() == Some("server/shuttingDown") {
            notice = msg.params;
        }
    }
    assert_eq!(c.closed, Some(1001));
    notice.expect("server/shuttingDown before the close")
}

#[tokio::test(flavor = "multi_thread")]
async fn the_admin_api_probes_harnesses_again_and_tells_the_clients() {
    let env = start(|_| {}).await;
    let admin = format!("Bearer {ADMIN}");
    let head = env
        .engine
        .stream_head(WORKSPACE_STREAM)
        .await
        .unwrap()
        .unwrap();
    let json = ("Content-Type", "application/json");
    for (body, headers) in [
        (
            &br#"{"harnessId":"fake"}"#[..],
            vec![("Authorization", admin.as_str()), json],
        ),
        (b"{}", vec![("Authorization", admin.as_str()), json]),
        // No body at all: every harness.
        (b"", vec![("Authorization", admin.as_str())]),
    ] {
        let (status, answer) = http(
            env.admin,
            "POST",
            "/v1/admin/harnesses/refresh",
            &headers,
            body,
        )
        .await;
        assert_eq!(status, 200, "{}", String::from_utf8_lossy(&answer));
        let list: aas_protocol::methods::HarnessListResult =
            serde_json::from_slice(&answer).unwrap();
        assert_eq!(list.harnesses.len(), 1);
        assert!(list.harnesses[0].available);
    }
    let batch = env
        .engine
        .read_batch(WORKSPACE_STREAM.to_owned(), head)
        .await
        .unwrap();
    assert!(
        batch.events.iter().any(|e| matches!(
            &e.event,
            Event::HarnessUpdated { harness } if harness.id == "fake"
        )),
        "clients hear about the refresh"
    );
    let (status, answer) = http(
        env.admin,
        "POST",
        "/v1/admin/harnesses/refresh",
        &[
            ("Authorization", &admin),
            ("Content-Type", "application/json"),
        ],
        br#"{"harnessId":"nope"}"#,
    )
    .await;
    assert_eq!(status, 404);
    assert_eq!(error_kind(&answer), "notFound");
    let (status, _) = http(
        env.admin,
        "POST",
        "/v1/admin/harnesses/refresh",
        &[("Authorization", "Bearer wrong")],
        b"",
    )
    .await;
    assert_eq!(status, 401);
    let (status, _) = http(
        env.addr,
        "POST",
        "/v1/admin/harnesses/refresh",
        &[("Authorization", &admin)],
        b"",
    )
    .await;
    assert_eq!(status, 404, "not routed on the public listener");
}

#[tokio::test(flavor = "multi_thread")]
async fn every_connection_is_told_the_same_shutdown_reason() {
    // Stopped while draining: every client hears "drain", never "shutdown".
    let mut env = start(|_| {}).await;
    let mut cs = clients(&env, 8).await;
    env.engine.wait_drained().await;
    env.stop_server().await;
    for c in &mut cs {
        let notice = shutdown_notice(c).await;
        assert_eq!(notice["reason"], "drain");
        assert_eq!(
            notice["restartExpected"], false,
            "`Server::run` has no supervisor that would restart it"
        );
    }
    // Stopped without a drain: "shutdown" for everyone.
    let mut env = start(|_| {}).await;
    let mut cs = clients(&env, 8).await;
    env.stop_server().await;
    // The server returned only after every connection had delivered its notice and closed.
    let (status, body) = http(
        env.admin,
        "GET",
        "/v1/admin/status",
        &[("Authorization", &format!("Bearer {ADMIN}"))],
        b"",
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(
        serde_json::from_slice::<Value>(&body).unwrap()["connectedDevices"],
        0
    );
    for c in &mut cs {
        assert_eq!(shutdown_notice(c).await["reason"], "shutdown");
    }
}

/// A connection whose client never reads (a phone that froze the app while its TCP
/// connection stays open): with a tiny receive window, whatever the server sends soon blocks.
async fn stalled_connection(addr: SocketAddr) -> TcpStream {
    let socket = tokio::net::TcpSocket::new_v4().unwrap();
    socket.set_recv_buffer_size(4096).unwrap();
    socket.connect(addr).await.unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_download_nobody_reads_does_not_hold_the_shutdown() {
    let limit = Duration::from_secs(1);
    let policy = ServerPolicy {
        writer_flush_timeout: Duration::from_millis(500),
        transport_shutdown_timeout: limit,
        ..ServerPolicy::default()
    };
    let mut env = start_with(|_| {}, policy).await;
    let token = pair(&env, "phone").await;
    let auth = format!("Bearer {token}");
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    png.resize(20 * 1024 * 1024, 0x55);
    let (status, body) = http(
        env.addr,
        "POST",
        "/v1/blobs",
        &[("Authorization", &auth), ("Content-Type", "image/png")],
        &png,
    )
    .await;
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    let id = serde_json::from_slice::<Value>(&body).unwrap()["blobId"]
        .as_str()
        .unwrap()
        .to_owned();
    let mut stalled = stalled_connection(env.addr).await;
    stalled
        .write_all(
            format!("GET /v1/blobs/{id} HTTP/1.1\r\nHost: x\r\nAuthorization: {auth}\r\n\r\n")
                .as_bytes(),
        )
        .await
        .unwrap();
    // The response is on its way and stuck in the full socket.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let stopping = tokio::time::Instant::now();
    env.stop_server().await;
    let took = stopping.elapsed();
    assert!(
        took >= limit,
        "the response in flight was given its time ({took:?})"
    );
    assert!(
        took < limit * 5,
        "the stalled response was dropped at the deadline ({took:?})"
    );
    drop(stalled);
}

#[tokio::test(flavor = "multi_thread")]
async fn run_until_delivers_the_callers_notice() {
    let env = start(|_| {}).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = Server::new(
        env.engine.clone(),
        ServerOptions {
            listen: addr,
            public_url: None,
            admin_token: ADMIN.into(),
        },
    );
    let (tx, rx) = tokio::sync::oneshot::channel::<aas_server::ShutdownNotice>();
    let serving = tokio::spawn(server.run_until(listener, async move { rx.await.unwrap() }));
    let token = pair(&env, "phone").await;
    let mut req = format!("ws://{addr}/v1/ws").into_client_request().unwrap();
    req.headers_mut()
        .insert("Authorization", format!("Bearer {token}").parse().unwrap());
    let (ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
    let mut c = Client {
        ws,
        next_id: 1,
        notifications: Vec::new(),
        closed: None,
    };
    c.init(None).await;
    // The engine is draining, but the caller decided: its notice wins.
    env.engine.wait_drained().await;
    tx.send(aas_server::ShutdownNotice {
        reason: aas_server::ShutdownReason::Shutdown,
        restart_expected: false,
    })
    .unwrap();
    let notice = shutdown_notice(&mut c).await;
    assert_eq!(notice["reason"], "shutdown");
    assert_eq!(notice["restartExpected"], false);
    tokio::time::timeout(Duration::from_secs(20), serving)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_one_batch_queue_still_delivers_long_streams() {
    // Tailers wait for room in the queue instead of buffering in memory.
    let env = start_with(
        |_| {},
        ServerPolicy {
            stream_batch_queue: 1,
            ..ServerPolicy::default()
        },
    )
    .await;
    let token = pair(&env, "phone").await;
    let mut c = connect(&env, &token).await.unwrap();
    c.init(None).await;
    let (_, thread) = setup_thread(&mut c, &env).await;
    let stream = format!("thread:{thread}");
    c.call(
        "subscribe",
        json!({"subscriptions": [{"stream": stream, "after": 0}]}),
    )
    .await
    .unwrap();
    c.call("turn/start", json!({"clientRequestId": "t1", "threadId": thread, "input": [{"type": "text", "text": "@stream 300"}]}))
        .await
        .unwrap();
    let events = c
        .events_until(&stream, |e| matches!(e, Event::TurnCompleted { .. }))
        .await;
    let text: String = events
        .iter()
        .filter_map(|e| match &e.event {
            Event::ItemDelta { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert!(text.contains("tok299 "));
}
