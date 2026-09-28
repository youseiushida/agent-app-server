//! Chaos test (design.md §16): a [`ReliableClient`] talks to the server through a
//! [`ChaosProxy`] that — driven by a seeded RNG — drops every connection, delays traffic, or
//! blackholes it for longer than `client_timeout`, while the fake harness (real agent
//! processes) runs long streaming turns, approvals and outputs too large to inline.
//!
//! Afterwards the client's state must equal the server's: exactly one turn per `turn/start`,
//! every event applied once and in order, items rebuilt from the events identical to
//! `thread/read`, and every approval resolved exactly once.

mod common;

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use aas_protocol::events::{Event, EventEnvelope};
use aas_protocol::ids::ItemId;
use aas_protocol::types::{
    ApprovalOptionKind, InteractionRequest, InteractionResolution, InteractionStatus, Item,
    ItemBody, ItemStatus, TurnStatus,
};
use aas_testkit::chaos::{ChaosProxy, Mode};
use aas_testkit::client::{ClientConfig, ClientState, ReliableClient};
use common::{HARNESS, TestServer, client_events};
use rand::rngs::StdRng;
use rand::{RngExt, SeedableRng};
use serde_json::{Value, json};
use tokio::sync::watch;

const HEARTBEAT: Duration = Duration::from_millis(200);
const CLIENT_TIMEOUT: Duration = Duration::from_millis(800);
const TURNS: u32 = 10;
/// Scripts the turns cycle through (starting at an offset derived from the seed).
const SCRIPTS: [&str; 3] = ["@stream 200 2", "@approve some-cmd", "@bigoutput 100000"];
/// Upper bound for any single step to get through the chaos.
const WAIT: Duration = Duration::from_secs(120);

/// Prints the seed when the test fails, so the run can be reproduced.
struct SeedReport(u64);

impl Drop for SeedReport {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!("chaos test FAILED with seed {}", self.0);
        }
    }
}

/// Log of this test binary (transport, engine, supervisor at INFO), shown when a step times out.
fn captured_log() -> Arc<Mutex<Vec<u8>>> {
    static LOG: OnceLock<Arc<Mutex<Vec<u8>>>> = OnceLock::new();
    LOG.get_or_init(|| {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let sink = buf.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_max_level(tracing::Level::INFO)
            .with_writer(move || LogSink(sink.clone()))
            .finish();
        tracing::subscriber::set_global_default(subscriber).expect("install log capture");
        buf
    })
    .clone()
}

struct LogSink(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for LogSink {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("log buffer").extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Client and server state for a failure message.
async fn dump(client: &ReliableClient, server: &TestServer, stream: Option<&str>) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "client: {}", client.describe());
    let _ = writeln!(
        out,
        "server: running turns {}, agent processes {}",
        server.engine.running_turns(),
        server.engine.running_processes()
    );
    if let Some(stream) = stream {
        let heads = server.heads(&["workspace", stream]).await;
        let _ = writeln!(out, "server heads: {heads:?}");
        let mine = client.with_state(|s| client_events(s, stream));
        for e in mine.iter().rev().take(12).rev() {
            let _ = writeln!(
                out,
                "  client {}..={} {}",
                e.seq_from.unwrap_or(e.seq),
                e.seq,
                e.event.type_name()
            );
        }
        for e in server.log(stream).await.iter().rev().take(12).rev() {
            let _ = writeln!(
                out,
                "  server {}..={} {}",
                e.seq_from.unwrap_or(e.seq),
                e.seq,
                e.event.type_name()
            );
        }
        if let Some(thread_id) = stream.strip_prefix("thread:") {
            let read = server.read_thread(thread_id).await;
            let _ = writeln!(
                out,
                "thread: {:?}, queued {}, paused {}",
                read.thread.status,
                read.queued.len(),
                read.thread.queue_paused
            );
            for t in &read.turns {
                let _ = writeln!(out, "  turn {} {:?} {:?}", t.index, t.status, t.error);
            }
            for i in &read.interactions {
                let _ = writeln!(out, "  interaction {} {:?}", i.id, i.status);
            }
        }
    }
    let log = String::from_utf8_lossy(&captured_log().lock().expect("log buffer")).into_owned();
    let lines: Vec<&str> = log.lines().collect();
    let _ = writeln!(out, "log tail (all seeds):");
    for line in &lines[lines.len().saturating_sub(150)..] {
        let _ = writeln!(out, "  {line}");
    }
    out
}

/// Waits for `pred`; on timeout, panics with a dump of the client and server state.
async fn wait(
    client: &ReliableClient,
    server: &TestServer,
    stream: Option<&str>,
    what: &str,
    pred: impl Fn(&ClientState) -> bool,
) {
    if !client.wait_for(WAIT, pred).await {
        panic!(
            "timed out waiting for {what}\n{}",
            dump(client, server, stream).await
        );
    }
}

/// Queues a mutating call and waits for its successful result.
async fn call(
    client: &ReliableClient,
    server: &TestServer,
    stream: Option<&str>,
    method: &str,
    params: Value,
) -> Value {
    let crid = client.mutate(method, params);
    wait(
        client,
        server,
        stream,
        &format!("the result of {method}"),
        |s| s.results.contains_key(&crid),
    )
    .await;
    client
        .with_state(|s| s.results[&crid].clone())
        .unwrap_or_else(|e| panic!("{method} failed: {e:?}"))
}

#[derive(Debug, Default)]
struct ChaosStats {
    drops: u32,
    delays: u32,
    blackholes: u32,
    passes: u32,
}

/// Sleeps for `d`; returns early with `true` once `stop` is set (or its sender is gone).
async fn stopped_within(stop: &mut watch::Receiver<bool>, d: Duration) -> bool {
    tokio::time::timeout(d, stop.wait_for(|s| *s)).await.is_ok()
}

/// Every 100–500 ms: drop all connections, delay traffic by 50–300 ms, blackhole it for longer
/// than the client timeout, or let it pass.
async fn chaos(proxy: Arc<ChaosProxy>, seed: u64, mut stop: watch::Receiver<bool>) -> ChaosStats {
    let mut rng = StdRng::seed_from_u64(seed);
    let mut stats = ChaosStats::default();
    loop {
        let pause = Duration::from_millis(rng.random_range(100..=500));
        if stopped_within(&mut stop, pause).await {
            break;
        }
        match rng.random_range(0..4u32) {
            0 => {
                proxy.drop_all();
                stats.drops += 1;
            }
            1 => {
                proxy.set_mode(Mode::Delay(Duration::from_millis(
                    rng.random_range(50..=300),
                )));
                stats.delays += 1;
            }
            2 => {
                proxy.set_mode(Mode::Blackhole);
                stats.blackholes += 1;
                let hold = CLIENT_TIMEOUT + Duration::from_millis(rng.random_range(100..=700));
                let stopped = stopped_within(&mut stop, hold).await;
                proxy.set_mode(Mode::Pass);
                if stopped {
                    break;
                }
            }
            _ => {
                proxy.set_mode(Mode::Pass);
                stats.passes += 1;
            }
        }
    }
    proxy.set_mode(Mode::Pass);
    stats
}

fn turn_completed(state: &ClientState, stream: &str, index: u32) -> bool {
    state.events.get(stream).is_some_and(|events| {
        events
            .iter()
            .any(|e| matches!(&e.event, Event::TurnCompleted { turn } if turn.index == index))
    })
}

/// Approval requests the client has seen but not answered yet: (interaction id, allow option).
fn unanswered(
    state: &ClientState,
    stream: &str,
    answered: &BTreeMap<String, String>,
) -> Vec<(String, String)> {
    let Some(events) = state.events.get(stream) else {
        return Vec::new();
    };
    events
        .iter()
        .filter_map(|e| match &e.event {
            Event::InteractionRequested { interaction }
                if !answered.contains_key(interaction.id.as_str()) =>
            {
                let InteractionRequest::Approval { options, .. } = &interaction.request else {
                    panic!("unexpected request {interaction:?}")
                };
                let allow = options
                    .iter()
                    .find(|o| o.kind == ApprovalOptionKind::AllowOnce)
                    .expect("allow option");
                Some((interaction.id.as_str().to_owned(), allow.id.clone()))
            }
            _ => None,
        })
        .collect()
}

/// Checks that `events` (as applied by the client) are strictly ordered and never overlap, and
/// that together they cover every event of the server's log.
///
/// Merged deltas (`seqFrom..=seq`) depend on where a batch started, so the client may hold a
/// range the log returns as one event in several pieces: coverage is checked on the union of
/// the client's ranges (contiguous runs), not event by event.
fn check_stream(name: &str, events: &[EventEnvelope], log: &[EventEnvelope]) {
    let mut last = 0u64;
    let mut runs: Vec<(u64, u64)> = Vec::new();
    for e in events {
        let from = e.seq_from.unwrap_or(e.seq);
        assert!(from <= e.seq, "{name}: seqFrom {from} > seq {}", e.seq);
        assert!(
            from > last,
            "{name}: event {from}..={} overlaps or precedes the previous seq {last}",
            e.seq
        );
        last = e.seq;
        match runs.last_mut() {
            Some(run) if run.1 + 1 == from => run.1 = e.seq,
            _ => runs.push((from, e.seq)),
        }
    }
    for e in log {
        let from = e.seq_from.unwrap_or(e.seq);
        let ok = runs.iter().any(|(a, b)| *a <= from && e.seq <= *b);
        assert!(
            ok,
            "{name}: server event {from}..={} ({}) never reached the client; client runs {runs:?}",
            e.seq,
            e.event.type_name()
        );
    }
}

/// The text a delta appends to (text of messages, output of commands).
fn content(body: &ItemBody) -> Option<&str> {
    match body {
        ItemBody::AgentMessage { text }
        | ItemBody::Reasoning { text }
        | ItemBody::UserMessage { text, .. } => Some(text),
        ItemBody::CommandExecution { output, .. } => Some(output),
        ItemBody::ToolCall { output, .. } => output.as_deref(),
        _ => None,
    }
}

/// Items rebuilt from thread events: `item/started` inserts, `item/delta` appends,
/// `item/updated` and `item/completed` replace. Before each completion the streamed content
/// must already equal the final one (nothing lost or doubled on the way).
fn rebuild_items(events: &[EventEnvelope]) -> Vec<Item> {
    let mut order: Vec<ItemId> = Vec::new();
    let mut items: HashMap<ItemId, Item> = HashMap::new();
    for e in events {
        match &e.event {
            Event::ItemStarted { item } => {
                assert!(
                    !items.contains_key(&item.id),
                    "item {} started twice",
                    item.id
                );
                order.push(item.id.clone());
                items.insert(item.id.clone(), item.clone());
            }
            Event::ItemDelta {
                item_id,
                field,
                text,
            } => {
                let item = items
                    .get_mut(item_id)
                    .unwrap_or_else(|| panic!("delta for unknown item {item_id}"));
                assert!(
                    item.body.append(*field, text),
                    "delta {field:?} does not apply to {}",
                    item.body.kind_str()
                );
            }
            Event::ItemUpdated { item } => {
                assert!(
                    items.contains_key(&item.id),
                    "update for unknown item {}",
                    item.id
                );
                items.insert(item.id.clone(), item.clone());
            }
            Event::ItemCompleted { item } => {
                let streamed = items
                    .get(&item.id)
                    .unwrap_or_else(|| panic!("completion of unknown item {}", item.id));
                assert_eq!(
                    content(&streamed.body),
                    content(&item.body),
                    "streamed content of {} differs from its final form",
                    item.id
                );
                items.insert(item.id.clone(), item.clone());
            }
            _ => {}
        }
    }
    order
        .into_iter()
        .map(|id| items.remove(&id).expect("item"))
        .collect()
}

async fn run(seed: u64) {
    let _report = SeedReport(seed);
    captured_log();
    let mut server = TestServer::start(|p| {
        p.heartbeat_interval = HEARTBEAT;
        p.client_timeout = CLIENT_TIMEOUT;
    })
    .await;
    let token = server.pair(&format!("chaos-{seed}")).await;
    let proxy = Arc::new(ChaosProxy::start(server.addr).await.expect("proxy"));
    let client = ReliableClient::start(ClientConfig {
        url: format!("ws://{}/v1/ws", proxy.addr()),
        token,
        backoff_max: Duration::from_millis(400),
    });
    let (stop_tx, stop_rx) = watch::channel(false);
    let chaos_task = tokio::spawn(chaos(proxy.clone(), seed, stop_rx));

    let project = call(
        &client,
        &server,
        None,
        "project/open",
        json!({"path": server.root.join("p").display().to_string()}),
    )
    .await;
    let project_id = project["project"]["id"]
        .as_str()
        .expect("project id")
        .to_owned();
    let created = call(
        &client,
        &server,
        None,
        "thread/create",
        json!({"projectId": project_id, "harnessId": HARNESS}),
    )
    .await;
    let thread_id = created["thread"]["id"]
        .as_str()
        .expect("thread id")
        .to_owned();
    let stream = format!("thread:{thread_id}");
    client.subscribe(&stream);

    // interaction id → clientRequestId of the one answer the client sent for it.
    let mut answered: BTreeMap<String, String> = BTreeMap::new();
    let mut scripts = Vec::new();
    for index in 0..TURNS {
        let script = SCRIPTS[(index as usize + seed as usize) % SCRIPTS.len()];
        scripts.push(script);
        let r = call(
            &client,
            &server,
            Some(&stream),
            "turn/start",
            json!({"threadId": thread_id, "input": [{"type": "text", "text": script}], "delivery": "auto"}),
        )
        .await;
        assert_eq!(r["disposition"], "started", "turn {index} ({script}): {r}");
        loop {
            let what = format!("turn {index} ({script}) to complete or ask");
            wait(&client, &server, Some(&stream), &what, |s| {
                turn_completed(s, &stream, index) || !unanswered(s, &stream, &answered).is_empty()
            })
            .await;
            let (done, requests) = client.with_state(|s| {
                (
                    turn_completed(s, &stream, index),
                    unanswered(s, &stream, &answered),
                )
            });
            for (interaction_id, option_id) in requests {
                let crid = client.mutate(
                    "interaction/respond",
                    json!({"interactionId": interaction_id, "resolution": {"kind": "approval", "optionId": option_id}}),
                );
                answered.insert(interaction_id, crid);
            }
            if done {
                break;
            }
        }
    }

    stop_tx.send_replace(true);
    let stats = chaos_task.await.expect("chaos task");
    for (interaction_id, crid) in &answered {
        wait(&client, &server, Some(&stream), "an answer's result", |s| {
            s.results.contains_key(crid)
        })
        .await;
        let r = client
            .with_state(|s| s.results[crid].clone())
            .unwrap_or_else(|e| panic!("answer to {interaction_id} failed: {e:?}"));
        assert_eq!(
            r["alreadyResolved"], false,
            "the client answered {interaction_id} once; the server applied it once: {r}"
        );
    }

    // Converge: the client's cursors reach the server's heads (and the heads stay put).
    let streams = ["workspace", stream.as_str()];
    loop {
        let heads = server.heads(&streams).await;
        wait(
            &client,
            &server,
            Some(&stream),
            "the cursors to reach the heads",
            |s| {
                heads
                    .iter()
                    .all(|(name, head)| s.cursors.get(name).copied().unwrap_or(0) >= *head)
            },
        )
        .await;
        if server.heads(&streams).await == heads {
            let cursors = client.with_state(|s| s.cursors.clone());
            for (name, head) in &heads {
                assert_eq!(cursors.get(name), Some(head), "cursor of {name}");
            }
            break;
        }
    }

    // Exactly one turn per turn/start, all completed, one user message each.
    let read = server.read_thread(&thread_id).await;
    assert_eq!(read.turns.len(), TURNS as usize, "turns on the server");
    assert!(
        read.turns.iter().all(|t| t.status == TurnStatus::Completed),
        "{:?}",
        read.turns
    );
    let user_texts: Vec<&str> = read
        .items
        .iter()
        .filter_map(|i| match &i.body {
            ItemBody::UserMessage { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        user_texts, scripts,
        "user inputs are neither lost nor duplicated"
    );

    // Every event reached the client once, in order.
    let (workspace_events, thread_events) =
        client.with_state(|s| (client_events(s, "workspace"), client_events(s, &stream)));
    check_stream(
        "workspace",
        &workspace_events,
        &server.log("workspace").await,
    );
    check_stream(&stream, &thread_events, &server.log(&stream).await);

    // Items rebuilt from the client's events equal the server's items.
    let rebuilt = rebuild_items(&thread_events);
    assert_eq!(rebuilt.len(), read.items.len(), "item count");
    for (mine, theirs) in rebuilt.iter().zip(&read.items) {
        assert_eq!(
            mine, theirs,
            "item rebuilt from events differs from thread/read"
        );
    }
    assert!(
        rebuilt.iter().all(|i| i.status == ItemStatus::Completed),
        "all items completed"
    );
    let truncated = rebuilt
        .iter()
        .filter(|i| {
            matches!(
                &i.body,
                ItemBody::CommandExecution {
                    output_truncated: true,
                    output_blob_id: Some(_),
                    ..
                }
            )
        })
        .count();
    let big_turns = scripts
        .iter()
        .filter(|s| s.starts_with("@bigoutput"))
        .count();
    assert_eq!(truncated, big_turns, "every large output went to a blob");

    // Every approval was requested once and resolved exactly once, with the client's answer.
    let approvals = scripts.iter().filter(|s| s.starts_with("@approve")).count();
    assert_eq!(answered.len(), approvals, "one answer per approval");
    assert_eq!(read.interactions.len(), approvals);
    for interaction in &read.interactions {
        let id = interaction.id.as_str();
        assert_eq!(
            interaction.status,
            InteractionStatus::Resolved,
            "{interaction:?}"
        );
        assert_eq!(
            interaction.resolution,
            Some(InteractionResolution::Approval {
                option_id: "allow".into(),
                feedback: None
            })
        );
        assert!(answered.contains_key(id), "the client answered {id}");
        let count =
            |pred: &dyn Fn(&Event) -> bool| thread_events.iter().filter(|e| pred(&e.event)).count();
        assert_eq!(
            count(
                &|e| matches!(e, Event::InteractionRequested { interaction: i } if i.id.as_str() == id)
            ),
            1,
            "{id} requested"
        );
        assert_eq!(
            count(
                &|e| matches!(e, Event::InteractionResolved { interaction: i } if i.id.as_str() == id)
            ),
            1,
            "{id} resolved"
        );
        let closed = workspace_events
            .iter()
            .filter(|e| matches!(&e.event, Event::InteractionClosed { interaction_id, status, .. } if interaction_id.as_str() == id && *status == InteractionStatus::Resolved))
            .count();
        assert_eq!(closed, 1, "{id} closed on the workspace stream");
        let approved = rebuilt.iter().any(|i| {
            Some(&i.id) == interaction.item_id.as_ref()
                && matches!(
                    &i.body,
                    ItemBody::CommandExecution {
                        exit_code: Some(0),
                        ..
                    }
                )
        });
        assert!(approved, "the approved command of {id} ran");
    }

    let (connects, resent) = client.with_state(|s| (s.connects, s.resent));
    eprintln!("chaos seed {seed}: {stats:?}; client connects={connects} resent={resent}");
    assert!(
        connects >= 3,
        "the chaos should have forced reconnects (connects={connects}, {stats:?})"
    );

    drop(client);
    server.shutdown().await;
    assert_eq!(server.supervisor.running_count(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn chaos_seed_1() {
    run(1).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn chaos_seed_2() {
    run(2).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn chaos_seed_3() {
    run(3).await;
}
