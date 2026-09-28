//! End to end: the fake harness runs as real `aas-dummy-agent` processes under the supervisor,
//! the engine is served over a real WebSocket, and a [`ReliableClient`] drives everything —
//! a normal turn, an approval round trip, a crashing agent followed by a resumed session, a
//! stop, and finally the daemon shutdown, after which no agent process may remain.

mod common;

use std::collections::BTreeSet;
use std::time::Duration;

use aas_protocol::events::{Event, EventEnvelope};
use aas_protocol::types::{
    ApprovalOptionKind, InteractionRequest, InteractionStatus, ItemBody, ItemStatus, ThreadStatus,
    Turn, TurnStatus,
};
use aas_testkit::client::{ClientConfig, ClientState, ReliableClient};
use aas_testkit::proc::{self, Proc};
use common::{HARNESS, TestServer, client_events, mutate};
use serde_json::json;

const WAIT: Duration = Duration::from_secs(60);

fn completed_turn(state: &ClientState, stream: &str, index: u32) -> Option<Turn> {
    state
        .events
        .get(stream)?
        .iter()
        .find_map(|e| match &e.event {
            Event::TurnCompleted { turn } if turn.index == index => Some(turn.clone()),
            _ => None,
        })
}

async fn wait_turn(client: &ReliableClient, stream: &str, index: u32) -> Turn {
    client
        .wait_until(WAIT, |s| completed_turn(s, stream, index).is_some())
        .await;
    client
        .with_state(|s| completed_turn(s, stream, index))
        .expect("completed turn")
}

async fn start_turn(client: &ReliableClient, thread_id: &str, text: &str) {
    let r = mutate(client, "turn/start", json!({"threadId": thread_id, "input": [{"type": "text", "text": text}], "delivery": "auto"}), WAIT).await;
    assert_eq!(r["disposition"], "started", "{r}");
}

/// Events of `stream` belonging to the turn with `index` (from its `turn/started` on).
fn turn_events(events: &[EventEnvelope], index: u32) -> Vec<EventEnvelope> {
    let start = events
        .iter()
        .position(|e| matches!(&e.event, Event::TurnStarted { turn } if turn.index == index))
        .unwrap_or_else(|| panic!("turn {index} never started"));
    events[start..].to_vec()
}

/// Agent message text of one turn, assembled from `item/started` and `item/delta` events only.
fn streamed_agent_text(events: &[EventEnvelope]) -> String {
    let mut agent_items = BTreeSet::new();
    let mut text = String::new();
    for e in events {
        match &e.event {
            Event::ItemStarted { item } if matches!(item.body, ItemBody::AgentMessage { .. }) => {
                agent_items.insert(item.id.clone());
            }
            Event::ItemDelta {
                item_id, text: t, ..
            } if agent_items.contains(item_id) => text.push_str(t),
            _ => {}
        }
    }
    text
}

/// The agent processes currently on the ledger; all of them are added to `seen`.
fn note_agents(server: &TestServer, seen: &mut BTreeSet<Proc>) -> Vec<Proc> {
    let agents = server.agent_processes();
    seen.extend(agents.iter().copied());
    agents
}

#[tokio::test(flavor = "multi_thread")]
async fn process_mode_turns_approvals_crash_stop_and_shutdown() {
    let mut server = TestServer::start(|_| {}).await;
    let token = server.pair("e2e-phone").await;
    let client = ReliableClient::start(ClientConfig {
        url: server.ws_url(),
        token,
        backoff_max: Duration::from_millis(500),
    });
    let mut seen: BTreeSet<Proc> = BTreeSet::new();

    let project = mutate(
        &client,
        "project/open",
        json!({"path": server.root.join("p").display().to_string()}),
        WAIT,
    )
    .await;
    let project_id = project["project"]["id"]
        .as_str()
        .expect("project id")
        .to_owned();
    let created = mutate(
        &client,
        "thread/create",
        json!({"projectId": project_id, "harnessId": HARNESS}),
        WAIT,
    )
    .await;
    let thread_id = created["thread"]["id"]
        .as_str()
        .expect("thread id")
        .to_owned();
    let stream = format!("thread:{thread_id}");
    client.subscribe(&stream);

    // 1. A normal turn streams the agent's answer.
    start_turn(&client, &thread_id, "hello over the wire").await;
    let turn = wait_turn(&client, &stream, 0).await;
    assert_eq!(turn.status, TurnStatus::Completed);
    let events = client.with_state(|s| client_events(s, &stream));
    assert_eq!(
        streamed_agent_text(&turn_events(&events, 0)),
        "echo: hello over the wire"
    );
    let first_agents = note_agents(&server, &mut seen);
    assert_eq!(
        first_agents.len(),
        1,
        "one agent process serves the thread: {first_agents:?}"
    );
    let first_agent = first_agents[0];
    assert!(
        server.ledger()[0].owner.as_deref() == Some(thread_id.as_str()),
        "the ledger records the owning thread"
    );
    let native_session = server
        .read_thread(&thread_id)
        .await
        .thread
        .native_session_id
        .expect("native session id");

    // 2. Approval round trip: the client sees the request in its thread stream and answers it.
    start_turn(&client, &thread_id, "@approve cargo build --release").await;
    let find_request = |s: &ClientState| {
        s.events.get(&stream)?.iter().find_map(|e| match &e.event {
            Event::InteractionRequested { interaction } => Some(interaction.clone()),
            _ => None,
        })
    };
    client.wait_until(WAIT, |s| find_request(s).is_some()).await;
    let interaction = client.with_state(find_request).expect("interaction");
    let InteractionRequest::Approval { options, .. } = &interaction.request else {
        panic!("expected an approval: {interaction:?}")
    };
    let allow = options
        .iter()
        .find(|o| o.kind == ApprovalOptionKind::AllowOnce)
        .expect("allow option");
    let answer = mutate(
        &client,
        "interaction/respond",
        json!({"interactionId": interaction.id, "resolution": {"kind": "approval", "optionId": allow.id}}),
        WAIT,
    )
    .await;
    assert_eq!(answer["alreadyResolved"], false);
    assert_eq!(answer["interaction"]["status"], "resolved");
    let turn = wait_turn(&client, &stream, 1).await;
    assert_eq!(turn.status, TurnStatus::Completed);
    let read = server.read_thread(&thread_id).await;
    assert!(
        read.items.iter().any(|i| i.turn_id == turn.id
            && matches!(&i.body, ItemBody::CommandExecution { command, exit_code: Some(0), output, .. }
                if command == "cargo build --release" && output == "ran cargo build --release\n")),
        "the approved command ran: {:?}",
        read.items
    );
    let resolved = read
        .interactions
        .iter()
        .find(|i| i.id == interaction.id)
        .expect("interaction in thread/read");
    assert_eq!(resolved.status, InteractionStatus::Resolved);
    assert_eq!(
        note_agents(&server, &mut seen),
        vec![first_agent],
        "the same process served the second turn"
    );

    // 3. The agent crashes: the turn fails, the process is gone, and the next turn resumes the
    //    same native session in a new process.
    start_turn(&client, &thread_id, "@crash 5").await;
    let turn = wait_turn(&client, &stream, 2).await;
    assert_eq!(turn.status, TurnStatus::Failed);
    let error = turn.error.expect("turn error");
    assert_eq!(error.kind, "agentExited");
    assert!(
        error.message.contains("exited with code 5"),
        "{}",
        error.message
    );
    assert!(
        proc::wait_all_dead(&[first_agent], Duration::from_secs(10)).is_empty(),
        "the crashed agent is gone"
    );
    let read = server.read_thread(&thread_id).await;
    assert_eq!(read.thread.status, ThreadStatus::Idle);
    assert_eq!(
        read.thread.last_error.as_ref().map(|e| e.kind.as_str()),
        Some("agentExited")
    );

    start_turn(&client, &thread_id, "back again").await;
    let turn = wait_turn(&client, &stream, 3).await;
    assert_eq!(turn.status, TurnStatus::Completed);
    let events = client.with_state(|s| client_events(s, &stream));
    assert_eq!(
        streamed_agent_text(&turn_events(&events, 3)),
        "echo: back again"
    );
    let read = server.read_thread(&thread_id).await;
    assert_eq!(
        read.thread.native_session_id.as_deref(),
        Some(native_session.as_str()),
        "resumed, not recreated"
    );
    assert!(
        read.thread.last_error.is_none(),
        "a successful turn clears the error"
    );
    let second_agents = note_agents(&server, &mut seen);
    assert_eq!(second_agents.len(), 1);
    assert_ne!(
        second_agents[0], first_agent,
        "a new process serves the resumed session"
    );

    // 4. thread/stop ends a running turn and its process.
    start_turn(&client, &thread_id, "@stream 100000 5").await;
    let streaming = |s: &ClientState| {
        s.events.get(&stream).is_some_and(|events| {
            events
                .iter()
                .skip_while(|e| !matches!(&e.event, Event::TurnStarted { turn } if turn.index == 4))
                .any(|e| matches!(e.event, Event::ItemDelta { .. }))
        })
    };
    client.wait_until(WAIT, streaming).await;
    let stopped = mutate(&client, "thread/stop", json!({"threadId": thread_id}), WAIT).await;
    assert_eq!(stopped["thread"]["status"], "idle", "{stopped}");
    let turn = wait_turn(&client, &stream, 4).await;
    assert_eq!(turn.status, TurnStatus::Interrupted);
    assert_eq!(turn.error.map(|e| e.kind), Some("stopped".to_owned()));
    assert!(
        proc::wait_all_dead(&second_agents, Duration::from_secs(10)).is_empty(),
        "the stopped agent is gone"
    );
    let read = server.read_thread(&thread_id).await;
    assert!(
        read.thread.last_error.is_none(),
        "a requested stop is not an error"
    );
    assert!(
        read.items
            .iter()
            .all(|i| i.status != ItemStatus::InProgress),
        "no item is left open"
    );
    assert!(
        server.agent_processes().is_empty(),
        "no agent process after the stop"
    );

    // 5. One more turn leaves a live process behind; the daemon shutdown must end it.
    start_turn(&client, &thread_id, "@text still here").await;
    let turn = wait_turn(&client, &stream, 5).await;
    assert_eq!(turn.status, TurnStatus::Completed);
    let last_agents = note_agents(&server, &mut seen);
    assert_eq!(last_agents.len(), 1);
    assert!(last_agents[0].alive());
    assert_eq!(server.supervisor.running_count(), 1);

    let turns: Vec<(u32, TurnStatus)> = server
        .read_thread(&thread_id)
        .await
        .turns
        .iter()
        .map(|t| (t.index, t.status))
        .collect();
    assert_eq!(
        turns,
        vec![
            (0, TurnStatus::Completed),
            (1, TurnStatus::Completed),
            (2, TurnStatus::Failed),
            (3, TurnStatus::Completed),
            (4, TurnStatus::Interrupted),
            (5, TurnStatus::Completed),
        ]
    );
    // project/open, thread/create, 6 × turn/start, interaction/respond, thread/stop.
    let failures: Vec<String> = client.with_state(|s| {
        s.results
            .iter()
            .filter_map(|(crid, r)| r.as_ref().err().map(|e| format!("{crid}: {e:?}")))
            .collect()
    });
    assert!(failures.is_empty(), "{failures:?}");
    assert_eq!(client.with_state(|s| s.results.len()), 10);

    server.shutdown().await;
    assert_eq!(
        server.supervisor.running_count(),
        0,
        "the supervisor still counts live children"
    );
    assert!(
        server.ledger().is_empty(),
        "the ledger still lists processes: {:?}",
        server.ledger()
    );
    let seen: Vec<Proc> = seen.into_iter().collect();
    assert_eq!(
        seen.len(),
        3,
        "before the crash, after the crash, after the stop: {seen:?}"
    );
    let survivors = proc::wait_all_dead(&seen, Duration::from_secs(10));
    assert!(
        survivors.is_empty(),
        "agent processes survived the shutdown: {survivors:?}"
    );
}
