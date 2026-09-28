//! Shared setup of the end-to-end and chaos tests: an [`Engine`] whose fake harness runs
//! `aas-dummy-agent agent` as real supervised processes, served by [`Server`] over real TCP,
//! plus server-side views used to check what the client ends up with.

// Each test crate includes this module and uses a different subset of it.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use aas_adapter_fake::FakeAdapter;
use aas_core::{Engine, EngineConfig, HarnessRegistry, Policy, RequestCtx};
use aas_harness::{AdapterContext, HarnessConfig, HarnessKind};
use aas_protocol::events::EventEnvelope;
use aas_protocol::methods::ThreadReadResult;
use aas_protocol::{ClientRequest, DeviceId, RpcError};
use aas_server::{Server, ServerOptions};
use aas_supervisor::{LedgerEntry, Supervisor, SupervisorPolicy};
use aas_testkit::client::{ClientState, ReliableClient};
use aas_testkit::proc::{self, Proc};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

pub const ADMIN_TOKEN: &str = "admin-secret-for-tests";
/// The harness id of the process-mode fake agent.
pub const HARNESS: &str = "fake";

pub struct TestServer {
    _dir: tempfile::TempDir,
    /// Canonical project root; `root/p` exists.
    pub root: PathBuf,
    pub data: PathBuf,
    pub addr: SocketAddr,
    pub engine: Arc<Engine>,
    pub supervisor: Supervisor,
    stop: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<std::io::Result<()>>>,
}

impl TestServer {
    /// Starts engine and server; `adjust` tunes the policy (short grace periods are preset).
    pub async fn start(adjust: impl FnOnce(&mut Policy)) -> Self {
        let dir = tempfile::tempdir().expect("temp dir");
        let data = dir.path().join("data");
        let root = dir.path().join("projects");
        std::fs::create_dir_all(root.join("p")).expect("project dir");
        let root = dunce::canonicalize(&root).expect("canonical root");
        let mut policy = Policy {
            stop_grace: Duration::from_millis(500),
            interrupt_grace: Duration::from_millis(1500),
            prevent_sleep_while_running: false,
            ..Policy::default()
        };
        adjust(&mut policy);
        let supervisor = Supervisor::new(
            &data.join("supervisor"),
            SupervisorPolicy {
                prevent_sleep: false,
                ..policy.supervisor_policy()
            },
        )
        .expect("supervisor");
        let ctx = AdapterContext {
            supervisor: supervisor.clone(),
            state_dir: data.join("adapters").join(HARNESS),
            policy: policy.adapter_policy(),
        };
        let harness = HarnessConfig {
            id: HARNESS.to_owned(),
            kind: HarnessKind::Fake,
            display_name: None,
            command: proc::bin_path("aas-dummy-agent").display().to_string(),
            args: Vec::new(),
            env: Default::default(),
            options: json!({ "mode": "process" }),
        };
        let registry = HarnessRegistry::new(vec![Arc::new(FakeAdapter::new(harness, ctx))]);
        let config = EngineConfig {
            data_dir: data.clone(),
            server_name: "test-pc".into(),
            hostname: "host".into(),
            project_roots: vec![root.clone()],
            policy,
            heuristics: Default::default(),
            git: None,
        };
        let engine = Engine::start(config, registry, supervisor.clone())
            .await
            .expect("engine");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listen");
        let addr = listener.local_addr().expect("local addr");
        let server = Server::new(
            engine.clone(),
            ServerOptions {
                listen: addr,
                public_url: Some(format!("ws://{addr}/v1/ws")),
                admin_token: ADMIN_TOKEN.into(),
            },
        );
        let (stop_tx, stop_rx) = oneshot::channel::<()>();
        let task = tokio::spawn(server.run(listener, async move {
            let _ = stop_rx.await;
        }));
        Self {
            _dir: dir,
            root,
            data,
            addr,
            engine,
            supervisor,
            stop: Some(stop_tx),
            task: Some(task),
        }
    }

    pub fn ws_url(&self) -> String {
        format!("ws://{}/v1/ws", self.addr)
    }

    /// Pairs a device through `POST /v1/pair`; returns its token.
    pub async fn pair(&self, device_name: &str) -> String {
        let (code, _) = self
            .engine
            .create_pairing_code()
            .await
            .expect("pairing code");
        let body = serde_json::to_vec(
            &json!({"code": code, "deviceName": device_name, "platform": "test"}),
        )
        .expect("json");
        let (status, resp) = http_post(self.addr, "/v1/pair", &body).await;
        assert_eq!(
            status,
            200,
            "pairing failed: {}",
            String::from_utf8_lossy(&resp)
        );
        let v: Value = serde_json::from_slice(&resp).expect("pair response");
        v["token"].as_str().expect("token").to_owned()
    }

    /// Calls the engine directly (for server-side views; not a client path).
    pub async fn call(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        let ctx = RequestCtx {
            device_id: DeviceId::from("dev_observer"),
        };
        self.engine
            .handle(&ctx, ClientRequest::parse(method, Some(params))?)
            .await
    }

    pub async fn read_thread(&self, thread_id: &str) -> ThreadReadResult {
        let v = self
            .call(
                "thread/read",
                json!({"threadId": thread_id, "limitTurns": 1000}),
            )
            .await
            .expect("thread/read");
        serde_json::from_value(v).expect("thread/read result")
    }

    /// Current heads of `streams`.
    pub async fn heads(&self, streams: &[&str]) -> BTreeMap<String, u64> {
        let mut heads = BTreeMap::new();
        for s in streams {
            let head = self
                .engine
                .stream_head(s)
                .await
                .expect("stream head")
                .unwrap_or_else(|| panic!("no stream {s}"));
            heads.insert((*s).to_owned(), head);
        }
        heads
    }

    /// The whole stored log of `stream`, as the server would replay it from 0.
    pub async fn log(&self, stream: &str) -> Vec<EventEnvelope> {
        let mut out = Vec::new();
        let mut cursor = 0;
        loop {
            let batch = self
                .engine
                .read_batch(stream.to_owned(), cursor)
                .await
                .expect("read log");
            if batch.events.is_empty() {
                return out;
            }
            cursor = batch.last_seq;
            out.extend(batch.events);
        }
    }

    /// Processes currently on the supervisor's ledger (the running agent processes).
    pub fn ledger(&self) -> Vec<LedgerEntry> {
        match std::fs::read(self.data.join("supervisor").join("children.json")) {
            Ok(bytes) => serde_json::from_slice(&bytes).expect("ledger"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => panic!("reading the ledger failed: {e}"),
        }
    }

    /// Agent processes on the ledger, identified by PID and creation time.
    pub fn agent_processes(&self) -> Vec<Proc> {
        self.ledger()
            .into_iter()
            .map(|e| Proc {
                pid: e.pid,
                created: e.created,
            })
            .collect()
    }

    /// Stops every agent process, then the transport.
    pub async fn shutdown(&mut self) {
        self.engine.shutdown(false).await;
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(task) = self.task.take() {
            tokio::time::timeout(Duration::from_secs(30), task)
                .await
                .expect("server stops")
                .expect("server task")
                .expect("server result");
        }
    }
}

/// Minimal HTTP/1.1 POST (one request per connection).
pub async fn http_post(addr: SocketAddr, path: &str, body: &[u8]) -> (u16, Vec<u8>) {
    let mut stream = TcpStream::connect(addr).await.expect("connect");
    let head = format!(
        "POST {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await.expect("write head");
    stream.write_all(body).await.expect("write body");
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.expect("read response");
    let split = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("header end");
    let head = String::from_utf8_lossy(&buf[..split]).to_string();
    let status: u16 = head
        .split_whitespace()
        .nth(1)
        .expect("status")
        .parse()
        .expect("status code");
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
        let line_end = raw
            .windows(2)
            .position(|w| w == b"\r\n")
            .expect("chunk size line");
        let size = usize::from_str_radix(
            std::str::from_utf8(&raw[..line_end]).expect("ascii").trim(),
            16,
        )
        .expect("chunk size");
        raw = &raw[line_end + 2..];
        if size == 0 {
            return out;
        }
        out.extend_from_slice(&raw[..size]);
        raw = &raw[size + 2..];
    }
}

/// Queues a mutating call and waits for its result; panics on an error.
pub async fn mutate(
    client: &ReliableClient,
    method: &str,
    params: Value,
    timeout: Duration,
) -> Value {
    let crid = client.mutate(method, params);
    client
        .result(&crid, timeout)
        .await
        .unwrap_or_else(|e| panic!("{method} failed: {e:?}"))
}

/// Events the client applied for `stream`.
pub fn client_events(state: &ClientState, stream: &str) -> Vec<EventEnvelope> {
    state.events.get(stream).cloned().unwrap_or_default()
}
