//! The forced stop with real processes (design.md §4.3): an agent that ignores the interrupt
//! (the fake agent's `@hang`, which also ignores its input ending) is terminated with its
//! whole tree once `interrupt_grace` and `stop_grace` have passed; the turn ends as
//! `interrupted` with `error.kind = "forced"`, the next turn runs on a new process, and no
//! agent process is left at the end.

mod common;

use std::time::{Duration, Instant};

use aas_protocol::events::Event;
use aas_protocol::types::{ItemBody, ThreadStatus, Turn, TurnStatus};
use aas_testkit::proc::{self, Proc};
use common::{HARNESS, TestServer};
use serde_json::{Value, json};

const WAIT: Duration = Duration::from_secs(60);

/// Polls the stored log of `stream` until an event matches; returns it.
async fn wait_event(server: &TestServer, stream: &str, pred: impl Fn(&Event) -> bool) -> Event {
    let deadline = Instant::now() + WAIT;
    loop {
        if let Some(e) = server
            .log(stream)
            .await
            .into_iter()
            .map(|e| e.event)
            .find(|e| pred(e))
        {
            return e;
        }
        assert!(Instant::now() < deadline, "timed out waiting on {stream}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn completed(server: &TestServer, stream: &str, index: u32) -> Turn {
    match wait_event(
        server,
        stream,
        |e| matches!(e, Event::TurnCompleted { turn } if turn.index == index),
    )
    .await
    {
        Event::TurnCompleted { turn } => turn,
        _ => unreachable!("matched above"),
    }
}

async fn wait_agents(server: &TestServer, count: usize) -> Vec<Proc> {
    let deadline = Instant::now() + WAIT;
    loop {
        let agents = server.agent_processes();
        if agents.len() >= count {
            return agents;
        }
        assert!(
            Instant::now() < deadline,
            "only {} agent process(es) on the ledger",
            agents.len()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn crid() -> String {
    uuid::Uuid::new_v4().to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn an_agent_that_ignores_the_interrupt_is_terminated_with_its_tree() {
    let interrupt_grace = Duration::from_millis(1000);
    let stop_grace = Duration::from_millis(500);
    let mut server = TestServer::start(|p| {
        p.interrupt_grace = interrupt_grace;
        p.stop_grace = stop_grace;
    })
    .await;
    let project: Value = server
        .call(
            "project/open",
            json!({"clientRequestId": crid(), "path": server.root.join("p").display().to_string()}),
        )
        .await
        .expect("project/open");
    let created: Value = server
        .call(
            "thread/create",
            json!({"clientRequestId": crid(), "projectId": project["project"]["id"], "harnessId": HARNESS,
                   "input": [{"type": "text", "text": "@text hanging now
@hang 600000"}]}),
        )
        .await
        .expect("thread/create");
    let thread_id = created["thread"]["id"]
        .as_str()
        .expect("thread id")
        .to_owned();
    let stream = format!("thread:{thread_id}");
    // The agent's own message shows that it received the input and is now hanging.
    wait_event(&server, &stream, |e| matches!(e, Event::ItemCompleted { item } if matches!(item.body, ItemBody::AgentMessage { .. }))).await;
    let first = wait_agents(&server, 1).await;
    assert!(first.iter().all(Proc::alive));

    let asked = Instant::now();
    let r = server
        .call(
            "turn/interrupt",
            json!({"clientRequestId": crid(), "threadId": thread_id}),
        )
        .await
        .expect("turn/interrupt");
    assert_eq!(r["interrupted"], true);
    let turn = completed(&server, &stream, 0).await;
    let took = asked.elapsed();
    assert_eq!(turn.status, TurnStatus::Interrupted);
    assert_eq!(
        turn.error.as_ref().map(|e| e.kind.as_str()),
        Some("forced"),
        "{turn:?}"
    );
    assert!(
        took >= interrupt_grace + stop_grace,
        "the agent had both grace periods ({took:?})"
    );
    let survivors = proc::wait_all_dead(&first, Duration::from_secs(10));
    assert!(
        survivors.is_empty(),
        "the agent process was terminated: {survivors:?}"
    );
    wait_event(
        &server,
        &stream,
        |e| matches!(e, Event::ThreadUpdated { thread } if thread.status == ThreadStatus::Idle),
    )
    .await;
    assert!(
        server.agent_processes().is_empty(),
        "nothing is left on the ledger"
    );

    // The next turn runs on a new process (the session is resumed).
    server
        .call("turn/start", json!({"clientRequestId": crid(), "threadId": thread_id, "input": [{"type": "text", "text": "still there?"}]}))
        .await
        .expect("turn/start");
    let next = completed(&server, &stream, 1).await;
    assert_eq!(next.status, TurnStatus::Completed);
    let second = wait_agents(&server, 1).await;
    assert!(second.iter().all(|p| !first.contains(p)), "a new process");
    let read = server.read_thread(&thread_id).await;
    assert!(read.items.iter().any(
        |i| matches!(&i.body, ItemBody::AgentMessage { text } if text == "echo: still there?")
    ));

    server.shutdown().await;
    let survivors = proc::wait_all_dead(&second, Duration::from_secs(10));
    assert!(
        survivors.is_empty(),
        "no agent survives the shutdown: {survivors:?}"
    );
    assert!(server.agent_processes().is_empty());
}
