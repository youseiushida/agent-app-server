//! Replays recorded `codex app-server` transcripts against the adapter over in-memory pipes.
//!
//! Script lines (see `tests/fixtures/README.md`):
//! * `{"s": msg, "respondsTo": recId?}` — the fake server writes `msg` (its `id` rewritten to
//!   the id the adapter used for the request recorded as `recId`);
//! * `{"c": {"method", "recId"?, "params"?}}` — the adapter must send this request or
//!   notification next (`params` is a subset check);
//! * `{"c": {"id", "result"}}` — the adapter must answer a server request exactly so;
//! * `{"exit": {"code"}}` — the fake process exits (stdout closes).
//!
//! After the last line the fake server behaves like app-server: it exits once its stdin closes.

// Each test crate that includes this module uses a different subset of it, so helpers unused
// by one crate are expected here (not a blanket allow for production code).
#![allow(dead_code)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use aas_adapter_codex::ProcessLink;
use aas_harness::{
    AdapterEvent, AdapterPolicy, SessionHandle, StartMode, StartRequest, ThreadId, ThreadSettings,
};
use aas_supervisor::{ExitInfo, StopReason};
use async_trait::async_trait;
use parking_lot::Mutex;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{Notify, watch};
use tokio::task::JoinHandle;

pub const STEP_TIMEOUT: Duration = Duration::from_secs(10);

pub fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
}

pub fn workspace() -> PathBuf {
    PathBuf::from(r"C:\WORKSPACE")
}

/// Process link driven by the script runner.
pub struct FakeLink {
    exit: watch::Sender<Option<ExitInfo>>,
    killed: Notify,
    kill_reason: Mutex<Option<StopReason>>,
}

impl FakeLink {
    pub fn new() -> Arc<Self> {
        let (exit, _) = watch::channel(None);
        Arc::new(Self {
            exit,
            killed: Notify::new(),
            kill_reason: Mutex::new(None),
        })
    }

    fn exited(&self, code: Option<i32>) {
        let stopped = *self.kill_reason.lock();
        self.exit.send_if_modified(|slot| {
            if slot.is_some() {
                return false;
            }
            *slot = Some(ExitInfo {
                code,
                stopped,
                stderr_tail: String::new(),
                exited_at_ms: 0,
            });
            true
        });
    }

    pub fn kill_reason(&self) -> Option<StopReason> {
        *self.kill_reason.lock()
    }
}

#[async_trait]
impl ProcessLink for FakeLink {
    async fn wait(&self) -> ExitInfo {
        let mut rx = self.exit.subscribe();
        loop {
            if let Some(info) = rx.borrow_and_update().clone() {
                return info;
            }
            rx.changed().await.expect("exit sender alive");
        }
    }

    async fn shutdown(&self, grace: Duration, reason: StopReason) -> ExitInfo {
        if let Ok(info) = tokio::time::timeout(grace, self.wait()).await {
            return info;
        }
        self.kill(reason);
        self.wait().await
    }

    fn kill(&self, reason: StopReason) {
        self.kill_reason.lock().get_or_insert(reason);
        self.killed.notify_one();
    }
}

fn check(pattern: &Value, actual: &Value, ids: &mut HashMap<String, Value>) -> Result<(), String> {
    if let Some(method) = pattern.get("method") {
        if actual.get("method") != Some(method) {
            return Err(format!("expected {method}, got {actual}"));
        }
        if actual.get("jsonrpc").is_some() {
            return Err(format!("codex messages must not carry jsonrpc: {actual}"));
        }
        if let Some(Value::Object(subset)) = pattern.get("params") {
            for (k, v) in subset {
                if actual["params"].get(k) != Some(v) {
                    return Err(format!(
                        "{method}: params.{k} expected {v}, got {}",
                        actual["params"]
                    ));
                }
            }
        }
        if let Some(Value::String(rec)) = pattern.get("recId") {
            let id = actual
                .get("id")
                .cloned()
                .ok_or_else(|| format!("{method} has no id"))?;
            ids.insert(rec.clone(), id);
        }
        Ok(())
    } else {
        if actual.get("id") != pattern.get("id") || actual.get("result") != pattern.get("result") {
            return Err(format!("expected response {pattern}, got {actual}"));
        }
        Ok(())
    }
}

/// Runs the script, then always closes stdout and publishes the exit (so `wait` never hangs,
/// whatever the outcome).
async fn run_script<R, W>(
    entries: Vec<Value>,
    reader: R,
    mut writer: W,
    link: Arc<FakeLink>,
) -> Result<(), String>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let (result, code) = run_inner(&entries, reader, &mut writer, &link).await;
    let _ = writer.shutdown().await;
    link.exited(code);
    result
}

async fn run_inner<R, W>(
    entries: &[Value],
    reader: R,
    writer: &mut W,
    link: &FakeLink,
) -> (Result<(), String>, Option<i32>)
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let mut lines = BufReader::new(reader).lines();
    let mut ids: HashMap<String, Value> = HashMap::new();
    for (n, entry) in entries.iter().enumerate() {
        if let Some(msg) = entry.get("s") {
            let mut msg = msg.clone();
            if let Some(Value::String(rec)) = entry.get("respondsTo") {
                match ids.get(rec) {
                    Some(id) => msg["id"] = id.clone(),
                    None => {
                        return (
                            Err(format!("line {n}: no request recorded as {rec}")),
                            Some(1),
                        );
                    }
                }
            }
            let mut line = serde_json::to_vec(&msg).unwrap();
            line.push(b'\n');
            if writer.write_all(&line).await.is_err() {
                return (
                    Err(format!("line {n}: adapter closed its input early")),
                    Some(0),
                );
            }
        } else if let Some(pattern) = entry.get("c") {
            let line = tokio::select! {
                read = tokio::time::timeout(STEP_TIMEOUT, lines.next_line()) => match read {
                    Ok(Ok(Some(line))) => line,
                    // Like app-server: stdin EOF ends the process normally.
                    Ok(Ok(None)) => return (Err(format!("line {n}: adapter closed stdin, expected {pattern}")), Some(0)),
                    Ok(Err(e)) => return (Err(format!("line {n}: {e}")), Some(1)),
                    Err(_) => return (Err(format!("line {n}: timed out waiting for {pattern}")), Some(1)),
                },
                _ = link.killed.notified() => {
                    return (Err(format!("line {n}: killed while expecting {pattern}")), Some(1));
                }
            };
            let actual: Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(e) => return (Err(format!("line {n}: {e}: {line}")), Some(1)),
            };
            if let Err(e) = check(pattern, &actual, &mut ids) {
                return (Err(format!("line {n}: {e}")), Some(1));
            }
        } else if let Some(exit) = entry.get("exit") {
            let code = exit.get("code").and_then(Value::as_i64).map(|c| c as i32);
            return (Ok(()), code);
        }
    }
    // Like app-server: exit once stdin closes (or when killed).
    tokio::select! {
        read = lines.next_line() => match read {
            Ok(Some(line)) => (Err(format!("unexpected message after the script ended: {line}")), Some(1)),
            _ => (Ok(()), Some(0)),
        },
        _ = link.killed.notified() => (Ok(()), Some(1)),
    }
}

pub struct Replay {
    pub handle: SessionHandle,
    pub runner: JoinHandle<Result<(), String>>,
    pub link: Arc<FakeLink>,
    pub events: Vec<AdapterEvent>,
}

pub fn policy() -> AdapterPolicy {
    AdapterPolicy {
        stop_grace: Duration::from_secs(2),
        max_line_bytes: 16 << 20,
        handshake_timeout: STEP_TIMEOUT,
    }
}

/// The entries of a script file.
pub fn script(script: &str) -> Vec<Value> {
    let text = std::fs::read_to_string(fixtures_dir().join(script))
        .unwrap_or_else(|e| panic!("{script}: {e}"));
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

pub async fn start(script: &str, mode: StartMode, settings: ThreadSettings) -> Replay {
    start_entries(self::script(script), mode, settings, policy()).await
}

/// Replays `entries` (e.g. a recorded script cut short) with `policy`.
pub async fn start_entries(
    entries: Vec<Value>,
    mode: StartMode,
    settings: ThreadSettings,
    policy: AdapterPolicy,
) -> Replay {
    let (adapter_writer, runner_reader) = tokio::io::duplex(1 << 20);
    let (runner_writer, adapter_reader) = tokio::io::duplex(1 << 20);
    let link = FakeLink::new();
    let runner = tokio::spawn(run_script(
        entries,
        runner_reader,
        runner_writer,
        link.clone(),
    ));
    let req = StartRequest {
        thread_id: ThreadId::generate(),
        cwd: workspace(),
        settings,
        mode,
    };
    let handle = aas_adapter_codex::testing::establish(
        adapter_reader,
        adapter_writer,
        link.clone(),
        req,
        policy,
    )
    .await
    .unwrap_or_else(|e| panic!("establish failed: {e}"));
    Replay {
        handle,
        runner,
        link,
        events: Vec::new(),
    }
}

impl Replay {
    pub async fn next(&mut self) -> Option<AdapterEvent> {
        let event = tokio::time::timeout(STEP_TIMEOUT, self.handle.events.recv())
            .await
            .expect("timed out waiting for an event");
        if let Some(e) = &event {
            self.events.push(e.clone());
        }
        event
    }

    /// Reads events until one matches `pred`; returns it.
    pub async fn until(&mut self, pred: impl Fn(&AdapterEvent) -> bool) -> AdapterEvent {
        loop {
            let event = self
                .next()
                .await
                .expect("events closed before the expected event");
            if pred(&event) {
                return event;
            }
        }
    }

    /// Reads events until the channel closes; returns the final `Exited` info.
    pub async fn until_closed(&mut self) -> ExitInfo {
        let mut exited = None;
        while let Some(event) = self.next().await {
            if let AdapterEvent::Exited { info } = &event {
                assert!(exited.is_none(), "Exited emitted twice");
                exited = Some(info.clone());
            } else {
                assert!(exited.is_none(), "event after Exited: {event:?}");
            }
        }
        exited.expect("channel closed without Exited")
    }

    pub async fn finish(self) -> Result<(), String> {
        tokio::time::timeout(STEP_TIMEOUT, self.runner)
            .await
            .expect("runner did not finish")
            .expect("runner panicked")
    }

    pub fn natives(&self) -> Vec<&AdapterEvent> {
        self.events
            .iter()
            .filter(|e| matches!(e, AdapterEvent::Native { .. }))
            .collect()
    }

    /// Concatenated deltas of an item.
    pub fn deltas_of(&self, key: &str) -> String {
        self.events
            .iter()
            .filter_map(|e| match e {
                AdapterEvent::ItemDelta { key: k, text, .. } if k == key => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }
}
