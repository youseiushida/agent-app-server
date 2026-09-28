//! The end of the Windows session (design.md §18.8): the engine gives the agents
//! `policy.end_session_stop_grace` and no more, and turns it could not record in time are
//! recorded as `systemShutdown` by the next start.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use aas_adapter_fake::FakeAdapter;
use aas_core::{Engine, EngineConfig, HarnessRegistry, Policy, RequestCtx};
use aas_harness::AdapterContext;
use aas_protocol::events::Event;
use aas_protocol::methods::{spec, *};
use aas_protocol::*;
use aas_supervisor::{Supervisor, SupervisorPolicy};

const WAIT: Duration = Duration::from_secs(20);

fn policy() -> Policy {
    Policy {
        // The staged stop of a CLI that does not react: far longer than the end of the session
        // may take.
        stop_grace: Duration::from_secs(30),
        end_session_stop_grace: Duration::from_millis(500),
        prevent_sleep_while_running: false,
        ..Policy::default()
    }
}

async fn start(data: &Path, root: &Path) -> Arc<Engine> {
    let policy = policy();
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
        state_dir: data.join("adapters").join("fake"),
        policy: policy.adapter_policy(),
    };
    let registry = HarnessRegistry::new(vec![Arc::new(FakeAdapter::in_process("fake", ctx))]);
    let config = EngineConfig {
        data_dir: data.to_path_buf(),
        server_name: "test".into(),
        hostname: "host".into(),
        project_roots: vec![root.to_path_buf()],
        policy,
        heuristics: Default::default(),
        git: None,
    };
    Engine::start(config, registry, supervisor).await.unwrap()
}

async fn call<M: MethodSpec>(engine: &Arc<Engine>, params: M::Params) -> M::Result {
    let ctx = RequestCtx {
        device_id: DeviceId::from("dev_test"),
    };
    let req = ClientRequest::parse(M::NAME, Some(serde_json::to_value(params).unwrap())).unwrap();
    serde_json::from_value(engine.handle(&ctx, req).await.unwrap()).unwrap()
}

fn crid() -> String {
    format!("crid-{}", ulid::Ulid::generate())
}

/// Waits until an agent message completes on the thread after `after` (the agent got its
/// input).
async fn wait_agent_message(engine: &Arc<Engine>, thread: &ThreadId, after: u64) {
    let stream = thread_stream(thread);
    let deadline = tokio::time::Instant::now() + WAIT;
    let mut cursor = after;
    loop {
        let mut rx = engine.subscribe_head(&stream);
        let batch = engine.read_batch(stream.clone(), cursor).await.unwrap();
        cursor = batch.last_seq;
        if batch.events.iter().any(|e| {
            matches!(&e.event, Event::ItemCompleted { item } if matches!(item.body, ItemBody::AgentMessage { .. }))
        }) {
            return;
        }
        if *rx.borrow_and_update() > cursor {
            continue;
        }
        tokio::time::timeout_at(deadline, rx.changed())
            .await
            .expect("the agent answers")
            .unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_end_of_the_session_waits_only_its_grace_and_the_next_start_records_the_rest() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let root: PathBuf = {
        let r = dir.path().join("projects");
        std::fs::create_dir_all(r.join("p")).unwrap();
        dunce::canonicalize(r).unwrap()
    };
    let engine = start(&data, &root).await;
    let project = call::<spec::ProjectOpen>(
        &engine,
        ProjectOpenParams {
            client_request_id: crid(),
            path: root.join("p").display().to_string(),
            name: None,
        },
    )
    .await
    .project;
    let created = call::<spec::ThreadCreate>(
        &engine,
        ThreadCreateParams {
            client_request_id: crid(),
            project_id: project.id,
            harness_id: "fake".into(),
            settings: None,
            workspace: None,
            title: None,
            // An agent that ignores the end of its input and every interrupt.
            input: Some(vec![InputPart::Text {
                text: "@text hanging now\n@hang 600000".into(),
            }]),
        },
    )
    .await;
    let thread = created.thread.id.clone();
    wait_agent_message(&engine, &thread, 0).await;

    let asked = std::time::Instant::now();
    engine.shutdown_for_end_session().await;
    let took = asked.elapsed();
    assert!(
        took >= policy().end_session_stop_grace && took < Duration::from_secs(5),
        "bounded by end_session_stop_grace, not stop_grace: {took:?}"
    );
    engine.close().await.unwrap();
    drop(engine);

    // The next start (after the next logon) records the turn with the reason it ended.
    let engine = start(&data, &root).await;
    let read = call::<spec::ThreadRead>(
        &engine,
        ThreadReadParams {
            thread_id: thread.clone(),
            limit_turns: None,
            before_turn_index: None,
        },
    )
    .await;
    let turn = &read.turns[0];
    assert_eq!(turn.status, TurnStatus::Interrupted);
    assert_eq!(
        turn.error.as_ref().map(|e| e.kind.as_str()),
        Some("systemShutdown")
    );
    engine.shutdown(false).await;
    engine.close().await.unwrap();
    drop(engine);

    // The record is used once: a later unexpected stop is a restart again.
    let engine = start(&data, &root).await;
    let head = engine
        .stream_head(&thread_stream(&thread))
        .await
        .unwrap()
        .unwrap();
    let again = call::<spec::TurnStart>(
        &engine,
        TurnStartParams {
            client_request_id: crid(),
            thread_id: thread.clone(),
            input: vec![InputPart::Text {
                text: "@text hanging again\n@hang 600000".into(),
            }],
            delivery: Delivery::Auto,
        },
    )
    .await;
    assert!(again.turn_id.is_some());
    wait_agent_message(&engine, &thread, head).await;
    engine.close().await.unwrap();
    drop(engine);
    let engine = start(&data, &root).await;
    let read = call::<spec::ThreadRead>(
        &engine,
        ThreadReadParams {
            thread_id: thread,
            limit_turns: None,
            before_turn_index: None,
        },
    )
    .await;
    let last = read.turns.last().unwrap();
    assert_eq!(
        last.error.as_ref().map(|e| e.kind.as_str()),
        Some("daemonRestarted")
    );
    engine.shutdown(false).await;
}
