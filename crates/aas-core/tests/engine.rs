//! Engine integration tests with the in-process fake agent (no tokens, no network).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use aas_adapter_fake::FakeAdapter;
use aas_core::{Engine, EngineConfig, HarnessRegistry, PairError, Policy, RequestCtx};
use aas_harness::AdapterContext;
use aas_protocol::events::{Event, EventEnvelope};
use aas_protocol::methods::{spec, *};
use aas_protocol::*;
use aas_supervisor::{Supervisor, SupervisorPolicy};
use serde_json::json;

/// Upper bound of every wait for an event. It only bounds how long a broken test hangs, so it
/// is generous: the git tests run about twenty git processes per turn (snapshots, diffs), and
/// with the whole suite running in parallel on a loaded Windows machine (antivirus scanning of
/// each new process) one of them was measured at over sixty seconds.
const WAIT: Duration = Duration::from_secs(120);

struct Env {
    _dir: tempfile::TempDir,
    root: PathBuf,
    data: PathBuf,
    engine: Arc<Engine>,
    ctx: RequestCtx,
    policy: Policy,
}

fn test_policy() -> Policy {
    Policy {
        stop_grace: Duration::from_millis(500),
        interrupt_grace: Duration::from_millis(1500),
        prevent_sleep_while_running: false,
        ..Policy::default()
    }
}

async fn start_engine(data: &Path, root: &Path, policy: Policy) -> Arc<Engine> {
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
        git: aas_supervisor::resolve_program("git").ok(),
    };
    Engine::start(config, registry, supervisor).await.unwrap()
}

async fn env_with(f: impl FnOnce(&mut Policy)) -> Env {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let root = dir.path().join("projects");
    std::fs::create_dir_all(&root).unwrap();
    let root = dunce::canonicalize(&root).unwrap();
    let mut policy = test_policy();
    f(&mut policy);
    let engine = start_engine(&data, &root, policy.clone()).await;
    Env {
        _dir: dir,
        root,
        data,
        engine,
        ctx: RequestCtx {
            device_id: DeviceId::from("dev_test"),
        },
        policy,
    }
}

async fn env() -> Env {
    env_with(|_| {}).await
}

fn crid() -> String {
    format!("crid-{}", ulid::Ulid::generate())
}

impl Env {
    async fn call<M: MethodSpec>(&self, params: M::Params) -> Result<M::Result, RpcError> {
        let req = ClientRequest::parse(M::NAME, Some(serde_json::to_value(params).unwrap()))?;
        let value = self.engine.handle(&self.ctx, req).await?;
        Ok(
            serde_json::from_value(value)
                .unwrap_or_else(|e| panic!("{}: bad result: {e}", M::NAME)),
        )
    }

    async fn project(&self, name: &str, git: bool) -> Project {
        let dir = self.root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        if git {
            run_git(&dir, &["init", "-q"]);
            configure_test_repo(&dir);
            std::fs::write(dir.join("README.md"), "hello\n").unwrap();
            run_git(&dir, &["add", "-A"]);
            run_git(&dir, &["commit", "-q", "-m", "init"]);
        }
        self.call::<spec::ProjectOpen>(ProjectOpenParams {
            client_request_id: crid(),
            path: dir.display().to_string(),
            name: None,
        })
        .await
        .unwrap()
        .project
    }

    async fn thread(&self, project: &Project) -> Thread {
        self.call::<spec::ThreadCreate>(ThreadCreateParams {
            client_request_id: crid(),
            project_id: project.id.clone(),
            harness_id: "fake".into(),
            settings: None,
            workspace: None,
            title: None,
            input: None,
        })
        .await
        .unwrap()
        .thread
    }

    async fn send(&self, thread: &ThreadId, text: &str, delivery: Delivery) -> TurnStartResult {
        self.call::<spec::TurnStart>(TurnStartParams {
            client_request_id: crid(),
            thread_id: thread.clone(),
            input: vec![InputPart::Text { text: text.into() }],
            delivery,
        })
        .await
        .unwrap()
    }

    /// Reads `stream` from `after` until `pred` matches; returns all events read.
    async fn wait_for(
        &self,
        stream: &str,
        after: u64,
        pred: impl Fn(&Event) -> bool,
    ) -> Vec<EventEnvelope> {
        let mut cursor = after;
        let mut seen = Vec::new();
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            let mut rx = self.engine.subscribe_head(stream);
            let batch = self
                .engine
                .read_batch(stream.to_owned(), cursor)
                .await
                .unwrap();
            cursor = batch.last_seq;
            for ev in batch.events {
                let done = pred(&ev.event);
                seen.push(ev);
                if done {
                    return seen;
                }
            }
            if *rx.borrow_and_update() > cursor {
                continue;
            }
            if tokio::time::timeout_at(deadline, rx.changed())
                .await
                .is_err()
            {
                panic!(
                    "timed out waiting on {stream}; saw: {:?}",
                    seen.iter().map(|e| e.event.type_name()).collect::<Vec<_>>()
                );
            }
        }
    }

    async fn wait_turn_done(&self, thread: &ThreadId, after: u64) -> (Turn, Vec<EventEnvelope>) {
        let events = self
            .wait_for(&thread_stream(thread), after, |e| {
                matches!(e, Event::TurnCompleted { .. })
            })
            .await;
        let turn = events
            .iter()
            .rev()
            .find_map(|e| match &e.event {
                Event::TurnCompleted { turn } => Some(turn.clone()),
                _ => None,
            })
            .unwrap();
        (turn, events)
    }

    async fn read(&self, thread: &ThreadId) -> ThreadReadResult {
        self.call::<spec::ThreadRead>(ThreadReadParams {
            thread_id: thread.clone(),
            before_turn_index: None,
            limit_turns: Some(50),
        })
        .await
        .unwrap()
    }
}

/// Gives the test repository at `dir` (just initialised) an identity and LF line endings. The
/// settings are appended to `.git/config` in one write: `git config` replaces the file through a
/// lock file and a rename for every setting, and on Windows that rename fails when another
/// process (a virus scanner indexing the fresh repository) has the file open at that moment.
fn configure_test_repo(dir: &Path) {
    use std::io::Write;
    let mut config = std::fs::OpenOptions::new()
        .append(true)
        .open(dir.join(".git").join("config"))
        .expect("the repository's config");
    config
        .write_all(b"[user]\n\temail = t@example.com\n\tname = t\n[core]\n\tautocrlf = false\n")
        .expect("writing the repository's config");
}

fn run_git(dir: &Path, args: &[&str]) {
    git_output(dir, args);
}

fn agent_text(items: &[Item]) -> String {
    items
        .iter()
        .filter_map(|i| match &i.body {
            ItemBody::AgentMessage { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("|")
}

#[tokio::test(flavor = "multi_thread")]
async fn turn_runs_and_persists_items() {
    let env = env().await;
    let project = env.project("p", false).await;
    let thread = env.thread(&project).await;
    assert_eq!(thread.status, ThreadStatus::Idle);
    let r = env.send(&thread.id, "hello there", Delivery::Auto).await;
    assert_eq!(r.disposition, Disposition::Started);
    let (turn, events) = env.wait_turn_done(&thread.id, 0).await;
    assert_eq!(turn.status, TurnStatus::Completed);
    assert!(turn.usage.is_some());
    assert!(
        events
            .iter()
            .any(|e| matches!(e.event, Event::ItemDelta { .. }))
    );

    let read = env.read(&thread.id).await;
    assert_eq!(read.turns.len(), 1);
    assert_eq!(read.thread.title, "hello there");
    assert!(matches!(read.items[0].body, ItemBody::UserMessage { .. }));
    assert_eq!(agent_text(&read.items), "echo: hello there");
    assert!(read.items.iter().all(|i| i.status == ItemStatus::Completed));
    assert_eq!(
        read.thread.status,
        ThreadStatus::Ready,
        "process stays up until idle reaping"
    );
    assert!(read.thread.usage.output_tokens > 0);
    // The workspace stream saw the thread change.
    let ws = env
        .engine
        .read_batch(WORKSPACE_STREAM.into(), 0)
        .await
        .unwrap();
    assert!(
        ws.events
            .iter()
            .any(|e| matches!(&e.event, Event::ThreadUpserted { thread: t } if t.id == thread.id))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn resent_requests_are_deduplicated_and_key_reuse_is_rejected() {
    let env = env().await;
    let project = env.project("p", false).await;
    let thread = env.thread(&project).await;
    let params = TurnStartParams {
        client_request_id: "same-key".into(),
        thread_id: thread.id.clone(),
        input: vec![InputPart::Text {
            text: "once".into(),
        }],
        delivery: Delivery::Auto,
    };
    let a = env.call::<spec::TurnStart>(params.clone()).await.unwrap();
    let b = env.call::<spec::TurnStart>(params.clone()).await.unwrap();
    assert_eq!(a, b, "a resend returns the stored result");
    env.wait_turn_done(&thread.id, 0).await;
    assert_eq!(env.read(&thread.id).await.turns.len(), 1, "no second turn");

    let mut other = params;
    other.input = vec![InputPart::Text {
        text: "different".into(),
    }];
    let err = env.call::<spec::TurnStart>(other).await.unwrap_err();
    assert_eq!(err.kind(), Some(ErrorKind::IdempotencyKeyReused));

    // Concurrent duplicates also execute once.
    let p2 = TurnStartParams {
        client_request_id: "concurrent".into(),
        thread_id: thread.id.clone(),
        input: vec![InputPart::Text {
            text: "twice?".into(),
        }],
        delivery: Delivery::Queue,
    };
    let (x, y) = tokio::join!(
        env.call::<spec::TurnStart>(p2.clone()),
        env.call::<spec::TurnStart>(p2)
    );
    assert_eq!(x.unwrap(), y.unwrap());
}

#[tokio::test(flavor = "multi_thread")]
async fn approvals_are_persistent_first_answer_wins() {
    let env = env().await;
    let project = env.project("p", false).await;
    let thread = env.thread(&project).await;
    env.send(&thread.id, "@approve cargo test", Delivery::Auto)
        .await;
    let ws = env
        .wait_for(WORKSPACE_STREAM, 0, |e| {
            matches!(e, Event::InteractionPending { .. })
        })
        .await;
    let interaction = ws
        .iter()
        .find_map(|e| match &e.event {
            Event::InteractionPending { interaction } => Some(interaction.clone()),
            _ => None,
        })
        .unwrap();
    assert!(matches!(
        interaction.request,
        InteractionRequest::Approval { .. }
    ));
    let pending = env
        .call::<spec::InteractionList>(InteractionListParams {
            status: Some(InteractionStatus::Pending),
        })
        .await
        .unwrap();
    assert_eq!(pending.interactions.len(), 1);
    let view = env
        .call::<spec::ThreadGet>(ThreadGetParams {
            thread_id: thread.id.clone(),
        })
        .await
        .unwrap()
        .thread;
    assert_eq!(view.pending_interactions, 1);

    // An invalid option is rejected and the request stays pending.
    let bad = env
        .call::<spec::InteractionRespond>(InteractionRespondParams {
            client_request_id: crid(),
            interaction_id: interaction.id.clone(),
            resolution: InteractionResolution::Approval {
                option_id: "nope".into(),
                feedback: None,
            },
        })
        .await
        .unwrap_err();
    assert_eq!(bad.kind(), Some(ErrorKind::InvalidParams));

    let first = env
        .call::<spec::InteractionRespond>(InteractionRespondParams {
            client_request_id: crid(),
            interaction_id: interaction.id.clone(),
            resolution: InteractionResolution::Approval {
                option_id: "allow".into(),
                feedback: None,
            },
        })
        .await
        .unwrap();
    assert!(!first.already_resolved);
    assert_eq!(first.interaction.status, InteractionStatus::Resolved);
    let second = env
        .call::<spec::InteractionRespond>(InteractionRespondParams {
            client_request_id: crid(),
            interaction_id: interaction.id.clone(),
            resolution: InteractionResolution::Approval {
                option_id: "deny".into(),
                feedback: None,
            },
        })
        .await
        .unwrap();
    assert!(second.already_resolved);
    assert_eq!(second.interaction.resolution, first.interaction.resolution);

    let (turn, _) = env.wait_turn_done(&thread.id, 0).await;
    assert_eq!(turn.status, TurnStatus::Completed);
    let read = env.read(&thread.id).await;
    assert!(read.items.iter().any(|i| matches!(
        &i.body,
        ItemBody::CommandExecution {
            exit_code: Some(0),
            ..
        }
    )));
    assert_eq!(read.thread.pending_interactions, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn queued_inputs_run_in_order_and_pause_after_interrupt() {
    let env = env().await;
    let project = env.project("p", false).await;
    let thread = env.thread(&project).await;
    env.send(&thread.id, "@sleep 400\n@text first", Delivery::Auto)
        .await;
    let q = env.send(&thread.id, "second", Delivery::Auto).await;
    assert_eq!(q.disposition, Disposition::Queued);
    let view = env
        .call::<spec::ThreadGet>(ThreadGetParams {
            thread_id: thread.id.clone(),
        })
        .await
        .unwrap()
        .thread;
    assert_eq!(view.queued_inputs, 1);
    // Both turns complete, in order, without further requests.
    let stream = thread_stream(&thread.id);
    env.wait_for(
        &stream,
        0,
        |e| matches!(e, Event::TurnCompleted { turn } if turn.index == 1),
    )
    .await;
    let read = env.read(&thread.id).await;
    assert_eq!(read.turns.len(), 2);
    assert_eq!(agent_text(&read.items), "first|echo: second");
    assert!(read.queued.is_empty());

    // Interrupting pauses the queue.
    let head = read.head;
    env.send(&thread.id, "@stream 10000 5", Delivery::Auto)
        .await;
    env.send(&thread.id, "later", Delivery::Queue).await;
    env.wait_for(&stream, head, |e| matches!(e, Event::ItemDelta { .. }))
        .await;
    let r = env
        .call::<spec::TurnInterrupt>(TurnInterruptParams {
            client_request_id: crid(),
            thread_id: thread.id.clone(),
        })
        .await
        .unwrap();
    assert!(r.interrupted);
    let (turn, _) = env.wait_turn_done(&thread.id, head).await;
    assert_eq!(turn.status, TurnStatus::Interrupted);
    let view = env.read(&thread.id).await;
    assert!(view.thread.queue_paused);
    assert_eq!(view.queued.len(), 1);
    // Resuming runs the queued input.
    let resumed = env
        .call::<spec::QueueResume>(QueueResumeParams {
            client_request_id: crid(),
            thread_id: thread.id.clone(),
        })
        .await
        .unwrap();
    assert!(resumed.turn_id.is_some());
    let (turn, _) = env.wait_turn_done(&thread.id, view.head).await;
    assert_eq!(turn.status, TurnStatus::Completed);
    assert!(!env.read(&thread.id).await.thread.queue_paused);
}

#[tokio::test(flavor = "multi_thread")]
async fn steering_adds_a_user_message_to_the_running_turn() {
    let env = env().await;
    let project = env.project("p", false).await;
    let thread = env.thread(&project).await;
    // The turn waits for the steer (no time window it must arrive in), and the steer is sent
    // once the agent has the turn's input (before that it would join the input instead).
    env.send(
        &thread.id,
        "@text waiting\n@await-steer\n@text done",
        Delivery::Auto,
    )
    .await;
    env.wait_for(
        &thread_stream(&thread.id),
        0,
        |e| matches!(e, Event::ItemCompleted { item } if matches!(&item.body, ItemBody::AgentMessage { text } if text == "waiting")),
    )
    .await;
    let s = env.send(&thread.id, "go faster", Delivery::Steer).await;
    assert_eq!(s.disposition, Disposition::Steered);
    env.wait_turn_done(&thread.id, 0).await;
    let read = env.read(&thread.id).await;
    assert_eq!(read.turns.len(), 1);
    assert!(read.items.iter().any(|i| matches!(
        &i.body,
        ItemBody::UserMessage {
            delivery: UserMessageDelivery::Steer,
            ..
        }
    )));
    assert!(read.items.iter().any(
        |i| matches!(&i.body, ItemBody::Notice { message, .. } if message.contains("go faster"))
    ));
}

/// A steer the turn did not take in before it ended comes back (the agent returns it before
/// its completion): its message is shown as not delivered, and it goes back to the queue, which
/// runs it as the next turn once the turn completed.
#[tokio::test(flavor = "multi_thread")]
async fn a_steer_the_turn_did_not_read_goes_back_to_the_queue() {
    let env = env().await;
    let project = env.project("p", false).await;
    let thread = env.thread(&project).await;
    env.send(
        &thread.id,
        "@text waiting\n@await-steer unread\n@text done",
        Delivery::Auto,
    )
    .await;
    // The agent has the turn's input (a steer before that would join the input instead).
    env.wait_for(
        &thread_stream(&thread.id),
        0,
        |e| matches!(e, Event::ItemCompleted { item } if matches!(&item.body, ItemBody::AgentMessage { text } if text == "waiting")),
    )
    .await;
    let s = env.send(&thread.id, "go faster", Delivery::Steer).await;
    assert_eq!(s.disposition, Disposition::Steered);
    // The returned steer runs as the next turn.
    let deadline = tokio::time::Instant::now() + WAIT;
    let read = loop {
        let read = env.read(&thread.id).await;
        if read.turns.len() == 2 && read.turns[1].status == TurnStatus::Completed {
            break read;
        }
        assert!(tokio::time::Instant::now() < deadline, "{read:#?}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert_eq!(read.turns[0].status, TurnStatus::Completed);
    let steered = read
        .items
        .iter()
        .find(|i| {
            matches!(
                &i.body,
                ItemBody::UserMessage {
                    delivery: UserMessageDelivery::Steer,
                    ..
                }
            )
        })
        .unwrap();
    assert_eq!(steered.status, ItemStatus::Declined);
    assert!(read.items.iter().any(|i| i.turn_id == read.turns[1].id
        && matches!(&i.body, ItemBody::AgentMessage { text } if text == "echo: go faster")));
    assert!(read.queued.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn crash_fails_the_turn_and_the_next_turn_resumes() {
    let env = env().await;
    let project = env.project("p", false).await;
    let thread = env.thread(&project).await;
    env.send(&thread.id, "@crash 5", Delivery::Auto).await;
    let (turn, _) = env.wait_turn_done(&thread.id, 0).await;
    assert_eq!(turn.status, TurnStatus::Failed);
    assert_eq!(turn.error.as_ref().unwrap().kind, "agentExited");
    let read = env.read(&thread.id).await;
    assert_eq!(read.thread.status, ThreadStatus::Idle);
    assert_eq!(read.thread.last_error.as_ref().unwrap().kind, "agentExited");
    let native = read.thread.native_session_id.clone().unwrap();

    env.send(&thread.id, "again", Delivery::Auto).await;
    let (turn, _) = env.wait_turn_done(&thread.id, read.head).await;
    assert_eq!(turn.status, TurnStatus::Completed);
    let read = env.read(&thread.id).await;
    assert_eq!(
        read.thread.native_session_id.unwrap(),
        native,
        "the session was resumed, not recreated"
    );
    assert!(read.thread.last_error.is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn stop_ends_the_process_and_interrupts_the_turn() {
    let env = env().await;
    let project = env.project("p", false).await;
    let thread = env.thread(&project).await;
    env.send(&thread.id, "@stream 100000 5", Delivery::Auto)
        .await;
    env.wait_for(&thread_stream(&thread.id), 0, |e| {
        matches!(e, Event::ItemDelta { .. })
    })
    .await;
    let stopped = env
        .call::<spec::ThreadStop>(ThreadStopParams {
            client_request_id: crid(),
            thread_id: thread.id.clone(),
        })
        .await
        .unwrap();
    assert_eq!(stopped.thread.status, ThreadStatus::Idle);
    let read = env.read(&thread.id).await;
    assert_eq!(read.turns[0].status, TurnStatus::Interrupted);
    assert_eq!(read.turns[0].error.as_ref().unwrap().kind, "stopped");
    assert!(
        read.thread.last_error.is_none(),
        "a requested stop is not an error"
    );
    assert!(
        read.items
            .iter()
            .all(|i| i.status != ItemStatus::InProgress)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn shutdown_returns_only_after_every_agent_exited() {
    let env = env().await;
    let project = env.project("p", false).await;
    let thread = env.thread(&project).await;
    env.send(&thread.id, "hello", Delivery::Auto).await;
    env.wait_turn_done(&thread.id, 0).await;
    assert_eq!(
        env.engine.running_processes(),
        1,
        "the agent stays up between turns"
    );
    // The engine drops its actor handles right after asking them to stop; the actors must
    // still answer only once their process is gone.
    env.engine.shutdown(false).await;
    assert_eq!(
        env.engine.running_processes(),
        0,
        "shutdown returned while an agent was still running"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn idle_processes_are_reaped_and_resumed() {
    let env = env_with(|p| p.idle_process_ttl = Duration::from_millis(300)).await;
    let project = env.project("p", false).await;
    let thread = env.thread(&project).await;
    env.send(&thread.id, "first", Delivery::Auto).await;
    let (_, _) = env.wait_turn_done(&thread.id, 0).await;
    env.wait_for(
        &thread_stream(&thread.id),
        0,
        |e| matches!(e, Event::ThreadUpdated { thread } if thread.status == ThreadStatus::Idle),
    )
    .await;
    let read = env.read(&thread.id).await;
    assert!(read.thread.last_error.is_none());
    env.send(&thread.id, "second", Delivery::Auto).await;
    let (turn, _) = env.wait_turn_done(&thread.id, read.head).await;
    assert_eq!(turn.status, TurnStatus::Completed);
}

#[tokio::test(flavor = "multi_thread")]
async fn capacity_limits_concurrent_processes() {
    let env = env_with(|p| p.max_running_processes = 1).await;
    let project = env.project("p", false).await;
    let a = env.thread(&project).await;
    let b = env.thread(&project).await;
    env.send(&a.id, "hello a", Delivery::Auto).await;
    env.wait_turn_done(&a.id, 0).await;
    // `a` still holds the only process slot, so `b` waits.
    env.send(&b.id, "hello b", Delivery::Auto).await;
    env.wait_for(
        &thread_stream(&b.id),
        0,
        |e| matches!(e, Event::ThreadUpdated { thread } if thread.status == ThreadStatus::Queued),
    )
    .await;
    env.call::<spec::ThreadStop>(ThreadStopParams {
        client_request_id: crid(),
        thread_id: a.id.clone(),
    })
    .await
    .unwrap();
    let (turn, _) = env.wait_turn_done(&b.id, 0).await;
    assert_eq!(turn.status, TurnStatus::Completed);
}

#[tokio::test(flavor = "multi_thread")]
async fn long_output_goes_to_a_blob() {
    let env = env_with(|p| p.max_inline_output_bytes = 4096).await;
    let project = env.project("p", false).await;
    let thread = env.thread(&project).await;
    env.send(&thread.id, "@bigoutput 50000", Delivery::Auto)
        .await;
    let (_, events) = env.wait_turn_done(&thread.id, 0).await;
    let delta_bytes: usize = events
        .iter()
        .filter_map(|e| match &e.event {
            Event::ItemDelta {
                field: DeltaField::Output,
                text,
                ..
            } => Some(text.len()),
            _ => None,
        })
        .sum();
    assert!(
        delta_bytes <= 4096,
        "inline deltas stay within the limit ({delta_bytes})"
    );
    let read = env.read(&thread.id).await;
    let (output, truncated, blob) = read
        .items
        .iter()
        .find_map(|i| match &i.body {
            ItemBody::CommandExecution {
                output,
                output_truncated,
                output_blob_id,
                ..
            } => Some((output.clone(), *output_truncated, output_blob_id.clone())),
            _ => None,
        })
        .unwrap();
    assert!(truncated);
    assert!(output.len() <= 4096);
    let (path, _) = env.engine.blob(&blob.unwrap()).await.unwrap().unwrap();
    assert_eq!(std::fs::metadata(path).unwrap().len(), 50000);
}

#[tokio::test(flavor = "multi_thread")]
async fn git_turn_diff_and_worktree_threads() {
    if aas_supervisor::resolve_program("git").is_err() {
        eprintln!("git not installed; skipping");
        return;
    }
    let env = env().await;
    let project = env.project("repo", true).await;
    assert!(project.git.is_repo);
    let thread = env.thread(&project).await;
    env.send(&thread.id, "@write src/new.txt hello world", Delivery::Auto)
        .await;
    let (turn, _) = env.wait_turn_done(&thread.id, 0).await;
    let events = env
        .wait_for(&thread_stream(&thread.id), 0, |e| {
            matches!(e, Event::TurnDiffUpdated { .. })
        })
        .await;
    let diff = events
        .iter()
        .find_map(|e| match &e.event {
            Event::TurnDiffUpdated { diff, .. } => Some(*diff),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        diff,
        DiffSummary {
            files: 1,
            insertions: 1,
            deletions: 0
        }
    );
    let d = env
        .call::<spec::ThreadDiff>(ThreadDiffParams {
            thread_id: thread.id.clone(),
            scope: DiffScope::Turn {
                turn_id: turn.id.clone(),
            },
        })
        .await
        .unwrap();
    assert_eq!(d.files[0].path, "src/new.txt");
    assert_eq!(d.files[0].kind, FileChangeKind::Add);
    assert!(d.patch.unwrap().contains("+hello world"));
    // The user's index was not touched.
    let status = std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(&project.path)
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&status.stdout).contains("?? src/"),
        "file stays untracked"
    );

    // Worktree thread: separate folder and branch; archive can remove it.
    let wt = env
        .call::<spec::ThreadCreate>(ThreadCreateParams {
            client_request_id: crid(),
            project_id: project.id.clone(),
            harness_id: "fake".into(),
            settings: None,
            workspace: Some(WorkspaceSpec::Worktree {
                base_ref: None,
                branch: None,
            }),
            title: Some("wt".into()),
            input: Some(vec![InputPart::Text {
                text: "@write wt.txt from worktree".into(),
            }]),
        })
        .await
        .unwrap();
    let Workspace::Worktree { path, branch, .. } = wt.thread.workspace.clone() else {
        panic!("expected worktree")
    };
    assert!(branch.starts_with("aas/"));
    env.wait_turn_done(&wt.thread.id, 0).await;
    assert!(Path::new(&path).join("wt.txt").exists());
    assert!(!Path::new(&project.path).join("wt.txt").exists());
    let archived = env
        .call::<spec::ThreadArchive>(ThreadArchiveParams {
            client_request_id: crid(),
            thread_id: wt.thread.id.clone(),
            archived: true,
            remove_worktree: true,
            force: true,
        })
        .await
        .unwrap();
    assert!(archived.thread.archived);
    assert!(!Path::new(&path).exists());
}

/// A new thread in another thread's workspace (`workspace: {kind: "thread"}`): it works in the
/// same worktree, like a fork of that thread; the worktree is not removed while both use it; a
/// thread of another project, an unknown thread and a removed worktree are refused.
#[tokio::test(flavor = "multi_thread")]
async fn a_new_thread_can_work_in_another_threads_worktree() {
    if aas_supervisor::resolve_program("git").is_err() {
        eprintln!("git not installed; skipping");
        return;
    }
    let env = env().await;
    let project = env.project("repo", true).await;
    let create = |workspace: WorkspaceSpec, input: Option<&str>| ThreadCreateParams {
        client_request_id: crid(),
        project_id: project.id.clone(),
        harness_id: "fake".into(),
        settings: None,
        workspace: Some(workspace),
        title: None,
        input: input.map(|t| vec![InputPart::Text { text: t.into() }]),
    };
    let wt = env
        .call::<spec::ThreadCreate>(create(
            WorkspaceSpec::Worktree {
                base_ref: None,
                branch: None,
            },
            None,
        ))
        .await
        .unwrap()
        .thread;
    let Workspace::Worktree { path, .. } = wt.workspace.clone() else {
        panic!("expected a worktree")
    };
    let same = env
        .call::<spec::ThreadCreate>(create(
            WorkspaceSpec::Thread {
                thread_id: wt.id.clone(),
            },
            Some("@write plan.txt implemented"),
        ))
        .await
        .unwrap()
        .thread;
    assert_eq!(
        (same.workspace.clone(), same.cwd.clone()),
        (wt.workspace.clone(), wt.cwd.clone())
    );
    env.wait_turn_done(&same.id, 0).await;
    assert!(Path::new(&path).join("plan.txt").exists());
    assert!(!Path::new(&project.path).join("plan.txt").exists());
    // The worktree is shared: removing it with either thread is refused.
    let e = env
        .call::<spec::ThreadArchive>(ThreadArchiveParams {
            client_request_id: crid(),
            thread_id: wt.id.clone(),
            archived: true,
            remove_worktree: true,
            force: true,
        })
        .await
        .unwrap_err();
    assert_eq!(e.kind(), Some(ErrorKind::InvalidState));
    // A thread in the project's folder shares the folder.
    let local = env.thread(&project).await;
    let beside = env
        .call::<spec::ThreadCreate>(create(
            WorkspaceSpec::Thread {
                thread_id: local.id.clone(),
            },
            None,
        ))
        .await
        .unwrap()
        .thread;
    assert_eq!(
        (beside.workspace, beside.cwd),
        (Workspace::Local, project.path.clone())
    );
    // Another project's thread, an unknown thread.
    let other = env.project("other", false).await;
    let other_thread = env.thread(&other).await;
    let e = env
        .call::<spec::ThreadCreate>(create(
            WorkspaceSpec::Thread {
                thread_id: other_thread.id.clone(),
            },
            None,
        ))
        .await
        .unwrap_err();
    assert_eq!(e.kind(), Some(ErrorKind::InvalidParams));
    let e = env
        .call::<spec::ThreadCreate>(create(
            WorkspaceSpec::Thread {
                thread_id: ThreadId::from("thr_missing"),
            },
            None,
        ))
        .await
        .unwrap_err();
    assert_eq!(e.kind(), Some(ErrorKind::NotFound));
    // A worktree that was removed.
    let gone = env
        .call::<spec::ThreadCreate>(create(
            WorkspaceSpec::Worktree {
                base_ref: None,
                branch: None,
            },
            None,
        ))
        .await
        .unwrap()
        .thread;
    env.call::<spec::ThreadArchive>(ThreadArchiveParams {
        client_request_id: crid(),
        thread_id: gone.id.clone(),
        archived: true,
        remove_worktree: true,
        force: true,
    })
    .await
    .unwrap();
    let e = env
        .call::<spec::ThreadCreate>(create(
            WorkspaceSpec::Thread {
                thread_id: gone.id.clone(),
            },
            None,
        ))
        .await
        .unwrap_err();
    assert_eq!(e.kind(), Some(ErrorKind::InvalidState));
}

/// A model that lists its permission modes (`Model.permissionModes`) runs in those only: the
/// combination is checked whenever a request sets the model or the mode (and at creation),
/// and a request that changes both together is accepted.
#[tokio::test(flavor = "multi_thread")]
async fn a_model_runs_only_in_the_permission_modes_it_lists() {
    let env = env().await;
    let project = env.project("p", false).await;
    let harness = env
        .call::<spec::HarnessList>(Empty {})
        .await
        .unwrap()
        .harnesses
        .into_iter()
        .find(|h| h.id == "fake")
        .unwrap();
    let lite = harness
        .models
        .iter()
        .find(|m| m.id == aas_adapter_fake::LIMITED_MODEL)
        .unwrap();
    assert_eq!(
        lite.permission_modes.as_deref(),
        Some(&["ask".to_owned()][..])
    );
    let settings = |model: Option<&str>, mode: Option<&str>| ThreadSettings {
        model: model.map(str::to_owned),
        effort: None,
        permission_mode: mode.map(str::to_owned),
    };
    let e = env
        .call::<spec::ThreadCreate>(ThreadCreateParams {
            client_request_id: crid(),
            project_id: project.id.clone(),
            harness_id: "fake".into(),
            settings: Some(settings(Some("fake-lite"), Some("auto"))),
            workspace: None,
            title: None,
            input: None,
        })
        .await
        .unwrap_err();
    assert_eq!(e.kind(), Some(ErrorKind::InvalidParams));
    let thread = env.thread(&project).await;
    let update = |s: ThreadSettings| ThreadUpdateParams {
        client_request_id: crid(),
        thread_id: thread.id.clone(),
        title: None,
        settings: Some(s),
        pinned: None,
        modes: None,
    };
    env.call::<spec::ThreadUpdate>(update(settings(None, Some("auto"))))
        .await
        .unwrap();
    // The model alone would leave the thread in a mode the model does not have.
    let e = env
        .call::<spec::ThreadUpdate>(update(settings(Some("fake-lite"), None)))
        .await
        .unwrap_err();
    assert_eq!(e.kind(), Some(ErrorKind::InvalidParams));
    assert_eq!(e.data.unwrap()["permissionMode"], "auto");
    // Both together.
    let r = env
        .call::<spec::ThreadUpdate>(update(settings(Some("fake-lite"), Some("ask"))))
        .await
        .unwrap();
    assert_eq!(r.thread.settings.model.as_deref(), Some("fake-lite"));
    let e = env
        .call::<spec::ThreadUpdate>(update(settings(None, Some("auto"))))
        .await
        .unwrap_err();
    assert_eq!(e.kind(), Some(ErrorKind::InvalidParams));
    // A model that runs in every mode.
    env.call::<spec::ThreadUpdate>(update(settings(Some("fake-slow"), Some("auto"))))
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn restart_recovers_running_state() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let root = dir.path().join("projects");
    std::fs::create_dir_all(root.join("p")).unwrap();
    let root = dunce::canonicalize(&root).unwrap();
    let ctx = RequestCtx {
        device_id: DeviceId::from("dev_test"),
    };
    let engine = start_engine(&data, &root, test_policy()).await;
    let call = |engine: Arc<Engine>, method: &'static str, params: serde_json::Value| {
        let ctx = ctx.clone();
        async move {
            engine
                .handle(&ctx, ClientRequest::parse(method, Some(params)).unwrap())
                .await
        }
    };
    let project: ProjectResult = serde_json::from_value(
        call(
            engine.clone(),
            "project/open",
            json!({"clientRequestId": "a", "path": root.join("p").display().to_string()}),
        )
        .await
        .unwrap(),
    )
    .unwrap();
    let created: ThreadCreateResult = serde_json::from_value(
        call(
            engine.clone(),
            "thread/create",
            json!({"clientRequestId": "b", "projectId": project.project.id, "harnessId": "fake", "input": [{"type": "text", "text": "@approve rm -rf /"}]}),
        )
        .await
        .unwrap(),
    )
    .unwrap();
    // Wait for the pending approval, then "crash" the daemon by dropping the engine without shutdown.
    let mut cursor = 0;
    loop {
        let batch = engine
            .read_batch(WORKSPACE_STREAM.into(), cursor)
            .await
            .unwrap();
        cursor = batch.last_seq;
        if batch
            .events
            .iter()
            .any(|e| matches!(e.event, Event::InteractionPending { .. }))
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    engine.shutdown(false).await;
    drop(engine);

    let engine = start_engine(&data, &root, test_policy()).await;
    let read: ThreadReadResult = serde_json::from_value(
        call(
            engine.clone(),
            "thread/read",
            json!({"threadId": created.thread.id}),
        )
        .await
        .unwrap(),
    )
    .unwrap();
    assert_eq!(read.thread.status, ThreadStatus::Idle);
    assert!(read.turns[0].status.is_terminal());
    assert!(
        read.interactions
            .iter()
            .all(|i| i.status != InteractionStatus::Pending)
    );
    assert!(
        read.items
            .iter()
            .all(|i| i.status != ItemStatus::InProgress)
    );
    // Resending the original request after the restart returns the stored result.
    let again: ThreadCreateResult = serde_json::from_value(
        call(
            engine.clone(),
            "thread/create",
            json!({"clientRequestId": "b", "projectId": project.project.id, "harnessId": "fake", "input": [{"type": "text", "text": "@approve rm -rf /"}]}),
        )
        .await
        .unwrap(),
    )
    .unwrap();
    assert_eq!(again.thread.id, created.thread.id);
    assert_eq!(again.turn_id, created.turn_id);
}

#[tokio::test(flavor = "multi_thread")]
async fn filesystem_api_and_project_lifecycle() {
    let env = env().await;
    let roots = env.call::<spec::FsRoots>(Empty {}).await.unwrap();
    assert_eq!(roots.roots.len(), 1);
    let made = env
        .call::<spec::ProjectCreate>(ProjectCreateParams {
            client_request_id: crid(),
            parent_path: env.root.display().to_string(),
            name: "fresh".into(),
            init: ProjectInit::GitInit,
        })
        .await
        .unwrap();
    let project = made.project.unwrap();
    assert!(Path::new(&project.path).join(".git").exists());
    let listed = env
        .call::<spec::FsList>(FsListParams {
            path: env.root.display().to_string(),
            include_files: false,
        })
        .await
        .unwrap();
    assert!(
        listed
            .entries
            .iter()
            .any(|e| e.name == "fresh" && e.is_git_repo == Some(true))
    );
    let outside = env
        .call::<spec::FsList>(FsListParams {
            path: env.data.display().to_string(),
            include_files: false,
        })
        .await
        .unwrap_err();
    assert_eq!(outside.kind(), Some(ErrorKind::PathNotAllowed));
    std::fs::write(Path::new(&project.path).join("hello.rs"), b"").unwrap();
    let found = env
        .call::<spec::FsSearch>(FsSearchParams {
            project_id: Some(project.id.clone()),
            thread_id: None,
            query: "hel".into(),
            limit: None,
        })
        .await
        .unwrap();
    assert_eq!(found.ranking, "heuristic:H1");
    assert_eq!(found.results[0].path, "hello.rs");
    // Opening the same folder again returns the same project.
    let again = env
        .call::<spec::ProjectOpen>(ProjectOpenParams {
            client_request_id: crid(),
            path: project.path.clone(),
            name: None,
        })
        .await
        .unwrap();
    assert_eq!(again.project.id, project.id);
    env.call::<spec::ProjectRemove>(ProjectRemoveParams {
        client_request_id: crid(),
        project_id: project.id.clone(),
    })
    .await
    .unwrap();
    let list = env
        .call::<spec::ProjectList>(ProjectListParams {
            include_archived: true,
        })
        .await
        .unwrap();
    assert!(list.projects.iter().all(|p| p.id != project.id));
    let _ = env.policy.clone();
}

#[tokio::test(flavor = "multi_thread")]
async fn pairing_and_device_revocation() {
    let env = env().await;
    let (code, _) = env.engine.create_pairing_code().await.unwrap();
    let paired = env
        .engine
        .pair(&code.to_lowercase(), "Pixel", "android")
        .await
        .unwrap();
    assert!(
        env.engine.pair(&code, "again", "android").await.is_err(),
        "codes are single use"
    );
    let device = env
        .engine
        .authenticate(&paired.token)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(device.id, paired.device_id);
    assert!(env.engine.authenticate("wrong").await.unwrap().is_none());
    let mut revocations = env.engine.revocations();
    assert!(env.engine.revoke_device(&paired.device_id).await.unwrap());
    assert_eq!(revocations.recv().await.unwrap(), paired.device_id);
    assert!(
        env.engine
            .authenticate(&paired.token)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn commands_merge_app_and_harness_entries() {
    let env = env().await;
    let project = env.project("p", false).await;
    let thread = env.thread(&project).await;
    let list = env
        .call::<spec::CommandList>(CommandListParams {
            thread_id: Some(thread.id.clone()),
            project_id: None,
            harness_id: None,
        })
        .await
        .unwrap();
    let names: Vec<&str> = list.commands.iter().map(|c| c.name.as_str()).collect();
    assert!(names.contains(&"model"));
    assert!(names.contains(&"stop"));
    assert!(names.contains(&"fake-help"));
    assert!(!names.contains(&"fork"), "the fake harness cannot fork");
    let fork = env
        .call::<spec::ThreadFork>(ThreadForkParams {
            client_request_id: crid(),
            thread_id: thread.id.clone(),
            at_turn_id: None,
            before: false,
        })
        .await
        .unwrap_err();
    assert_eq!(fork.kind(), Some(ErrorKind::CapabilityUnsupported));
}

fn git_available() -> bool {
    aas_supervisor::resolve_program("git").is_ok()
}

async fn wait_diff(env: &Env, thread: &ThreadId, turn: &TurnId) -> DiffSummary {
    let events = env
        .wait_for(
            &thread_stream(thread),
            0,
            |e| matches!(e, Event::TurnDiffUpdated { turn_id, .. } if turn_id == turn),
        )
        .await;
    events
        .iter()
        .find_map(|e| match &e.event {
            Event::TurnDiffUpdated { turn_id, diff } if turn_id == turn => Some(*diff),
            _ => None,
        })
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_paused_queue_does_not_keep_the_process_alive() {
    let env = env_with(|p| p.idle_process_ttl = Duration::from_millis(300)).await;
    let project = env.project("p", false).await;
    let thread = env.thread(&project).await;
    let stream = thread_stream(&thread.id);
    env.send(&thread.id, "@stream 10000 5", Delivery::Auto)
        .await;
    env.send(&thread.id, "later", Delivery::Queue).await;
    env.wait_for(&stream, 0, |e| matches!(e, Event::ItemDelta { .. }))
        .await;
    env.call::<spec::TurnInterrupt>(TurnInterruptParams {
        client_request_id: crid(),
        thread_id: thread.id.clone(),
    })
    .await
    .unwrap();
    let (turn, events) = env.wait_turn_done(&thread.id, 0).await;
    assert_eq!(turn.status, TurnStatus::Interrupted);
    let after = events.last().unwrap().seq;
    // The queue waits for the user; the idle process is reaped meanwhile and its slot freed.
    env.wait_for(
        &stream,
        after,
        |e| matches!(e, Event::ThreadUpdated { thread } if thread.status == ThreadStatus::Idle),
    )
    .await;
    assert_eq!(env.engine.running_processes(), 0);
    let read = env.read(&thread.id).await;
    assert!(read.thread.queue_paused);
    assert_eq!(read.queued.len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_later_turn_takes_its_own_base_snapshot() {
    if !git_available() {
        eprintln!("git not installed; skipping");
        return;
    }
    let env = env().await;
    let project = env.project("repo", true).await;
    let thread = env.thread(&project).await;
    let first = env
        .send(&thread.id, "@write a.txt one", Delivery::Auto)
        .await
        .turn_id
        .unwrap();
    assert_eq!(wait_diff(&env, &thread.id, &first).await.files, 1);
    // The user edits a file between the turns (the process stays alive).
    std::fs::write(
        Path::new(&project.path).join("README.md"),
        "edited by the user\n",
    )
    .unwrap();
    let second = env
        .send(&thread.id, "just talk", Delivery::Auto)
        .await
        .turn_id
        .unwrap();
    assert_eq!(
        wait_diff(&env, &thread.id, &second).await,
        DiffSummary {
            files: 0,
            insertions: 0,
            deletions: 0
        }
    );
    let d = env
        .call::<spec::ThreadDiff>(ThreadDiffParams {
            thread_id: thread.id.clone(),
            scope: DiffScope::Turn { turn_id: second },
        })
        .await
        .unwrap();
    assert!(
        d.files.is_empty(),
        "the edit of the user is not the agent's: {:?}",
        d.files
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_finished_turn_diff_ignores_later_changes() {
    if !git_available() {
        eprintln!("git not installed; skipping");
        return;
    }
    let env = env().await;
    let project = env.project("repo", true).await;
    let thread = env.thread(&project).await;
    let turn = env
        .send(&thread.id, "@write a.txt one", Delivery::Auto)
        .await
        .turn_id
        .unwrap();
    env.wait_turn_done(&thread.id, 0).await;
    // Right after `turn/completed` (before the diff summary is computed) the folder changes.
    std::fs::write(Path::new(&project.path).join("later.txt"), "later\n").unwrap();
    let d = env
        .call::<spec::ThreadDiff>(ThreadDiffParams {
            thread_id: thread.id.clone(),
            scope: DiffScope::Turn { turn_id: turn },
        })
        .await
        .unwrap();
    let paths: Vec<&str> = d.files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(paths, vec!["a.txt"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn cloning_into_the_folder_of_a_removed_project_registers_it_again() {
    if !git_available() {
        eprintln!("git not installed; skipping");
        return;
    }
    let env = env().await;
    let origin = env.project("origin", true).await;
    let dest = env.project("dest", false).await;
    env.call::<spec::ProjectRemove>(ProjectRemoveParams {
        client_request_id: crid(),
        project_id: dest.id.clone(),
    })
    .await
    .unwrap();
    std::fs::remove_dir_all(&dest.path).unwrap();
    let head = env
        .engine
        .read_batch(WORKSPACE_STREAM.into(), 0)
        .await
        .unwrap()
        .head;
    let made = env
        .call::<spec::ProjectCreate>(ProjectCreateParams {
            client_request_id: crid(),
            parent_path: env.root.display().to_string(),
            name: "dest".into(),
            init: ProjectInit::GitClone {
                url: origin.path.clone(),
            },
        })
        .await
        .unwrap();
    let op = made.operation.expect("clone operation");
    let events = env
        .wait_for(WORKSPACE_STREAM, head, |e| {
            matches!(e, Event::OperationUpdated { operation } if operation.id == op.id && operation.status != OperationStatus::Running)
        })
        .await;
    let finished = events
        .iter()
        .rev()
        .find_map(|e| match &e.event {
            Event::OperationUpdated { operation } if operation.id == op.id => {
                Some(operation.clone())
            }
            _ => None,
        })
        .unwrap();
    assert_eq!(
        finished.status,
        OperationStatus::Succeeded,
        "{:?}",
        finished.message
    );
    assert_eq!(
        finished.project_id.as_ref(),
        Some(&dest.id),
        "the removed project of that folder is back"
    );
    let list = env
        .call::<spec::ProjectList>(ProjectListParams {
            include_archived: false,
        })
        .await
        .unwrap();
    assert!(list.projects.iter().any(|p| p.id == dest.id));
}

// ----- pinning, queue editing, context usage ---------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn threads_can_be_pinned_and_unpinned() {
    let env = env().await;
    let project = env.project("p", false).await;
    let thread = env.thread(&project).await;
    assert!(!thread.pinned);
    let head = env
        .engine
        .read_batch(WORKSPACE_STREAM.into(), 0)
        .await
        .unwrap()
        .head;
    let updated = env
        .call::<spec::ThreadUpdate>(ThreadUpdateParams {
            client_request_id: crid(),
            thread_id: thread.id.clone(),
            title: None,
            settings: None,
            pinned: Some(true),
            modes: None,
        })
        .await
        .unwrap();
    assert!(updated.thread.pinned);
    assert_eq!(updated.settings_outcome, None);
    assert_eq!(
        updated.thread.last_activity_at, thread.last_activity_at,
        "pinning is not activity"
    );
    env.wait_for(
        WORKSPACE_STREAM,
        head,
        |e| matches!(e, Event::ThreadUpserted { thread: t } if t.id == thread.id && t.pinned),
    )
    .await;
    let got = env
        .call::<spec::ThreadGet>(ThreadGetParams {
            thread_id: thread.id.clone(),
        })
        .await
        .unwrap()
        .thread;
    assert!(got.pinned);
    let unpinned = env
        .call::<spec::ThreadUpdate>(ThreadUpdateParams {
            client_request_id: crid(),
            thread_id: thread.id.clone(),
            title: Some("Renamed".into()),
            settings: None,
            pinned: Some(false),
            modes: None,
        })
        .await
        .unwrap();
    assert!(!unpinned.thread.pinned);
    assert_eq!(unpinned.thread.title, "Renamed");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_queued_input_can_be_edited_in_place() {
    let env = env().await;
    let project = env.project("p", false).await;
    let thread = env.thread(&project).await;
    env.send(&thread.id, "@sleep 800\n@text first", Delivery::Auto)
        .await;
    let a = env
        .send(&thread.id, "second", Delivery::Queue)
        .await
        .queued_id
        .unwrap();
    let b = env
        .send(&thread.id, "third", Delivery::Queue)
        .await
        .queued_id
        .unwrap();
    let edited = env
        .call::<spec::QueueUpdate>(QueueUpdateParams {
            client_request_id: crid(),
            thread_id: thread.id.clone(),
            queued_id: a.clone(),
            input: vec![InputPart::Text {
                text: "second, edited".into(),
            }],
        })
        .await
        .unwrap();
    assert!(edited.updated);
    let queued = env.read(&thread.id).await.queued;
    assert_eq!(
        queued.iter().map(|q| q.id.clone()).collect::<Vec<_>>(),
        vec![a.clone(), b.clone()],
        "the edit keeps the order"
    );
    assert_eq!(queued[0].preview, "second, edited");
    // Invalid input is refused like a turn/start input.
    let err = env
        .call::<spec::QueueUpdate>(QueueUpdateParams {
            client_request_id: crid(),
            thread_id: thread.id.clone(),
            queued_id: b.clone(),
            input: vec![InputPart::Text { text: "   ".into() }],
        })
        .await
        .unwrap_err();
    assert_eq!(err.kind(), Some(ErrorKind::InvalidParams));
    // Both run in order with the edited text.
    env.wait_for(
        &thread_stream(&thread.id),
        0,
        |e| matches!(e, Event::TurnCompleted { turn } if turn.index == 2),
    )
    .await;
    let read = env.read(&thread.id).await;
    assert_eq!(
        agent_text(&read.items),
        "first|echo: second, edited|echo: third"
    );
    // An entry that already left the queue is not updated.
    let gone = env
        .call::<spec::QueueUpdate>(QueueUpdateParams {
            client_request_id: crid(),
            thread_id: thread.id.clone(),
            queued_id: a,
            input: vec![InputPart::Text {
                text: "too late".into(),
            }],
        })
        .await
        .unwrap();
    assert!(!gone.updated);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_queued_input_can_be_sent_now() {
    let env = env().await;
    let project = env.project("p", false).await;
    let thread = env.thread(&project).await;
    let stream = thread_stream(&thread.id);
    // The turn waits for the steer, then runs until it is interrupted (no time windows).
    let running = env
        .send(
            &thread.id,
            "@text waiting\n@await-steer\n@sleep 600000\n@text done",
            Delivery::Auto,
        )
        .await
        .turn_id
        .unwrap();
    env.wait_for(
        &stream,
        0,
        |e| matches!(e, Event::ItemCompleted { item } if matches!(&item.body, ItemBody::AgentMessage { text } if text == "waiting")),
    )
    .await;
    let q = env
        .send(&thread.id, "hurry up", Delivery::Queue)
        .await
        .queued_id
        .unwrap();
    let keep = env
        .send(&thread.id, "afterwards", Delivery::Queue)
        .await
        .queued_id
        .unwrap();
    // While a turn runs, the entry is steered into it and leaves the queue.
    let sent = env
        .call::<spec::QueueSteer>(QueueSteerParams {
            client_request_id: crid(),
            thread_id: thread.id.clone(),
            queued_id: q.clone(),
        })
        .await
        .unwrap();
    assert_eq!(sent.disposition, Some(Disposition::Steered));
    assert_eq!(sent.turn_id.as_ref(), Some(&running));
    let read = env.read(&thread.id).await;
    assert_eq!(
        read.queued.iter().map(|x| x.id.clone()).collect::<Vec<_>>(),
        vec![keep.clone()]
    );
    assert!(read
        .items
        .iter()
        .any(|i| matches!(&i.body, ItemBody::UserMessage { text, delivery: UserMessageDelivery::Steer, .. } if text == "hurry up")));
    // Gone from the queue: nothing happens.
    let again = env
        .call::<spec::QueueSteer>(QueueSteerParams {
            client_request_id: crid(),
            thread_id: thread.id.clone(),
            queued_id: q,
        })
        .await
        .unwrap();
    assert_eq!(
        again,
        QueueSteerResult {
            disposition: None,
            turn_id: None
        }
    );
    // With no turn running (the queue paused by an interrupt), the entry starts a turn.
    env.call::<spec::TurnInterrupt>(TurnInterruptParams {
        client_request_id: crid(),
        thread_id: thread.id.clone(),
    })
    .await
    .unwrap();
    let (turn, _) = env.wait_turn_done(&thread.id, 0).await;
    assert_eq!(turn.status, TurnStatus::Interrupted);
    let paused = env.read(&thread.id).await;
    assert!(paused.thread.queue_paused);
    let started = env
        .call::<spec::QueueSteer>(QueueSteerParams {
            client_request_id: crid(),
            thread_id: thread.id.clone(),
            queued_id: keep,
        })
        .await
        .unwrap();
    assert_eq!(started.disposition, Some(Disposition::Started));
    let (turn, _) = env.wait_turn_done(&thread.id, paused.head).await;
    assert_eq!(Some(&turn.id), started.turn_id.as_ref());
    assert_eq!(turn.status, TurnStatus::Completed);
    let read = env.read(&thread.id).await;
    assert!(read.queued.is_empty());
    assert!(!read.thread.queue_paused);
    assert!(agent_text(&read.items).ends_with("echo: afterwards"));
}

#[tokio::test(flavor = "multi_thread")]
async fn context_usage_reported_by_the_harness_reaches_turns_and_threads() {
    let env = env().await;
    let project = env.project("p", false).await;
    let thread = env.thread(&project).await;
    let reported = Some(ContextUsage {
        used_tokens: 1200,
        window_tokens: 8000,
    });
    let turn_id = env
        .send(
            &thread.id,
            "@context 1200 8000\n@sleep 200\n@text done",
            Delivery::Auto,
        )
        .await
        .turn_id
        .unwrap();
    let stream = thread_stream(&thread.id);
    let events = env
        .wait_for(&stream, 0, |e| matches!(e, Event::TurnUsageUpdated { .. }))
        .await;
    let live = events
        .iter()
        .find_map(|e| match &e.event {
            Event::TurnUsageUpdated { turn_id: t, usage } if t == &turn_id => Some(*usage),
            _ => None,
        })
        .unwrap();
    assert_eq!(live.context, reported);
    // The running turn carries it in thread/read too.
    let read = env.read(&thread.id).await;
    let running = read.turns.iter().find(|t| t.id == turn_id).unwrap();
    if running.status == TurnStatus::Running {
        assert_eq!(running.usage.and_then(|u| u.context), reported);
    }
    let (turn, _) = env.wait_turn_done(&thread.id, 0).await;
    assert_eq!(turn.usage.and_then(|u| u.context), reported);
    assert_eq!(env.read(&thread.id).await.thread.usage.context, reported);
    // A turn without a report has none, and the thread keeps the last reported occupancy.
    let head = env.read(&thread.id).await.head;
    env.send(&thread.id, "plain", Delivery::Auto).await;
    let (plain, _) = env.wait_turn_done(&thread.id, head).await;
    assert_eq!(plain.usage.and_then(|u| u.context), None);
    assert_eq!(env.read(&thread.id).await.thread.usage.context, reported);
}

#[tokio::test(flavor = "multi_thread")]
async fn server_status_reports_the_sleep_policy() {
    let env = env_with(|p| p.prevent_sleep_while_running = false).await;
    let status = env.call::<spec::ServerStatus>(Empty {}).await.unwrap();
    assert!(!status.prevent_sleep_while_running);
}

// ----- clone operations -------------------------------------------------------------------------

/// A local HTTP server for clone tests: every request is answered with 401 (`unauthorized`),
/// or never answered at all.
async fn http_server(unauthorized: bool) -> std::net::SocketAddr {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let mut request = Vec::new();
                let mut buf = [0u8; 4096];
                while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                    match socket.read(&mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => request.extend_from_slice(&buf[..n]),
                    }
                }
                if unauthorized {
                    let response = "HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"test\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.shutdown().await;
                } else {
                    // Keep the connection open without answering: the client waits.
                    let _ = socket.read(&mut buf).await;
                    std::future::pending::<()>().await;
                }
            });
        }
    });
    addr
}

impl Env {
    async fn clone_into(&self, name: &str, url: &str) -> Operation {
        self.call::<spec::ProjectCreate>(ProjectCreateParams {
            client_request_id: crid(),
            parent_path: self.root.display().to_string(),
            name: name.into(),
            init: ProjectInit::GitClone { url: url.into() },
        })
        .await
        .unwrap()
        .operation
        .expect("a clone operation")
    }

    /// Updates of `op` on the workspace stream from `after` until it has ended, and its final state.
    async fn wait_operation_end(
        &self,
        op: &OperationId,
        after: u64,
    ) -> (Vec<Operation>, Operation) {
        let events = self
            .wait_for(WORKSPACE_STREAM, after, |e| {
                matches!(e, Event::OperationUpdated { operation } if &operation.id == op && operation.status.is_terminal())
            })
            .await;
        let updates: Vec<Operation> = events
            .into_iter()
            .filter_map(|e| match e.event {
                Event::OperationUpdated { operation } if &operation.id == op => Some(operation),
                _ => None,
            })
            .collect();
        let last = updates.last().cloned().unwrap();
        (updates, last)
    }

    /// Entries of the projects root whose name starts with `prefix`.
    fn root_entries(&self, prefix: &str) -> Vec<String> {
        std::fs::read_dir(&self.root)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(prefix))
            .collect()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_clone_reports_progress_and_lands_only_when_complete() {
    if !git_available() {
        eprintln!("git not installed; skipping");
        return;
    }
    let env = env_with(|p| p.operation_progress_interval = Duration::ZERO).await;
    let origin = env.project("origin", true).await;
    let url = format!("file:///{}", origin.path.replace('\\', "/"));
    let head = env
        .engine
        .read_batch(WORKSPACE_STREAM.into(), 0)
        .await
        .unwrap()
        .head;
    let op = env.clone_into("copy", &url).await;
    assert_eq!(op.status, OperationStatus::Running);
    let (updates, done) = env.wait_operation_end(&op.id, head).await;
    assert_eq!(
        done.status,
        OperationStatus::Succeeded,
        "{:?}",
        done.message
    );
    assert!(
        done.progress.is_none(),
        "progress is only shown while running"
    );
    assert!(
        updates
            .iter()
            .filter(|o| o.progress.is_some())
            .all(|o| o.status == OperationStatus::Running),
        "progress never follows the end: {updates:?}"
    );
    assert!(env.root.join("copy").join("README.md").exists());
    assert!(
        env.root_entries(".copy.").is_empty(),
        "the temporary folder is gone: {:?}",
        env.root_entries(".")
    );
    let listed = env.call::<spec::OperationList>(Empty {}).await.unwrap();
    assert_eq!(listed.operations[0], done);
    // Cancelling a finished operation returns it unchanged.
    let after = env
        .call::<spec::OperationCancel>(OperationCancelParams {
            client_request_id: crid(),
            operation_id: op.id.clone(),
        })
        .await
        .unwrap();
    assert_eq!(after.operation, done);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_hanging_clone_can_be_cancelled_and_leaves_nothing_behind() {
    if !git_available() {
        eprintln!("git not installed; skipping");
        return;
    }
    let env = env().await;
    let addr = http_server(false).await;
    let head = env
        .engine
        .read_batch(WORKSPACE_STREAM.into(), 0)
        .await
        .unwrap()
        .head;
    let op = env
        .clone_into("stuck", &format!("http://{addr}/repo.git"))
        .await;
    // git's first line arrives verbatim while the clone waits for the server.
    let events = env
        .wait_for(WORKSPACE_STREAM, head, |e| {
            matches!(e, Event::OperationUpdated { operation } if operation.id == op.id && operation.progress.is_some())
        })
        .await;
    let progress = events
        .iter()
        .find_map(|e| match &e.event {
            Event::OperationUpdated { operation } if operation.id == op.id => {
                operation.progress.clone()
            }
            _ => None,
        })
        .unwrap();
    assert!(progress.starts_with("Cloning into"), "{progress}");
    let started = std::time::Instant::now();
    let cancelled = env
        .call::<spec::OperationCancel>(OperationCancelParams {
            client_request_id: crid(),
            operation_id: op.id.clone(),
        })
        .await
        .unwrap();
    assert_eq!(cancelled.operation.status, OperationStatus::Cancelled);
    assert!(cancelled.operation.finished_at.is_some());
    assert!(cancelled.operation.progress.is_none());
    assert!(
        started.elapsed() < Duration::from_secs(30),
        "cancelling does not wait for the clone"
    );
    assert!(!env.root.join("stuck").exists(), "no target folder");
    assert!(
        env.root_entries(".stuck.").is_empty(),
        "no temporary folder: {:?}",
        env.root_entries(".")
    );
    let (_, last) = env.wait_operation_end(&op.id, head).await;
    assert_eq!(last, cancelled.operation);
    let unknown = env
        .call::<spec::OperationCancel>(OperationCancelParams {
            client_request_id: crid(),
            operation_id: OperationId::from("op_unknown"),
        })
        .await
        .unwrap_err();
    assert_eq!(unknown.kind(), Some(ErrorKind::NotFound));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_clone_that_needs_credentials_fails_instead_of_prompting() {
    if !git_available() {
        eprintln!("git not installed; skipping");
        return;
    }
    let env = env_with(|p| p.clone_timeout = Duration::from_secs(300)).await;
    let addr = http_server(true).await;
    let head = env
        .engine
        .read_batch(WORKSPACE_STREAM.into(), 0)
        .await
        .unwrap()
        .head;
    let started = std::time::Instant::now();
    let op = env
        .clone_into("private", &format!("http://{addr}/private.git"))
        .await;
    let (_, done) = env.wait_operation_end(&op.id, head).await;
    assert_eq!(done.status, OperationStatus::Failed);
    let message = done.message.unwrap_or_default();
    assert!(message.starts_with("git clone failed"), "{message}");
    // git's own words for GIT_TERMINAL_PROMPT=0 (messages are in English: LC_ALL=C).
    assert!(message.contains("terminal prompts disabled"), "{message}");
    assert!(
        started.elapsed() < Duration::from_secs(60),
        "no credential prompt was waited for ({:?})",
        started.elapsed()
    );
    assert!(!env.root.join("private").exists());
    assert!(env.root_entries(".private.").is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_daemon_stop_ends_running_clones_and_a_restart_cleans_up_after_a_crash() {
    if !git_available() {
        eprintln!("git not installed; skipping");
        return;
    }
    let env = env().await;
    let addr = http_server(false).await;
    let head = env
        .engine
        .read_batch(WORKSPACE_STREAM.into(), 0)
        .await
        .unwrap()
        .head;
    let op = env
        .clone_into("interrupted", &format!("http://{addr}/repo.git"))
        .await;
    env.wait_for(WORKSPACE_STREAM, head, |e| {
        matches!(e, Event::OperationUpdated { operation } if operation.id == op.id && operation.progress.is_some())
    })
    .await;
    env.engine.shutdown(false).await;
    let ops = env
        .call::<spec::OperationList>(Empty {})
        .await
        .unwrap()
        .operations;
    let stopped = ops.iter().find(|o| o.id == op.id).unwrap();
    assert_eq!(stopped.status, OperationStatus::Failed);
    assert_eq!(
        stopped.message.as_deref(),
        Some("the daemon stopped during this operation")
    );
    assert!(env.root_entries(".interrupted.").is_empty());

    // A crash leaves the record running and the temporary folder behind: the next start ends
    // the operation and removes the folder.
    let leftover = env.root.join(".crashed.aas-clone-op_crash");
    std::fs::create_dir_all(leftover.join("objects")).unwrap();
    std::fs::write(leftover.join("objects").join("x"), b"partial").unwrap();
    {
        let db = rusqlite::Connection::open(env.data.join("aas.db")).unwrap();
        db.execute(
            "INSERT INTO operations (id, kind, status, message, started_at, work_dir) VALUES ('op_crash', 'gitClone', 'running', 'Cloning', 1, ?1)",
            [leftover.display().to_string()],
        )
        .unwrap();
    }
    let engine = start_engine(&env.data, &env.root, env.policy.clone()).await;
    let listed = engine
        .handle(
            &env.ctx,
            ClientRequest::parse("operation/list", None).unwrap(),
        )
        .await
        .unwrap();
    let ops = serde_json::from_value::<OperationListResult>(listed)
        .unwrap()
        .operations;
    let crashed = ops.iter().find(|o| o.id.as_str() == "op_crash").unwrap();
    assert_eq!(crashed.status, OperationStatus::Failed);
    assert!(
        !leftover.exists(),
        "the partial clone was removed at startup"
    );
    engine.shutdown(false).await;
}

// ----- forced stop -------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn an_agent_that_ignores_the_interrupt_is_stopped_and_the_turn_is_forced() {
    let grace = Duration::from_millis(400);
    let env = env_with(|p| {
        p.interrupt_grace = grace;
        p.stop_grace = Duration::from_millis(300);
    })
    .await;
    let project = env.project("p", false).await;
    let thread = env.thread(&project).await;
    let stream = thread_stream(&thread.id);
    env.send(
        &thread.id,
        "@text hanging now
@hang 600000",
        Delivery::Auto,
    )
    .await;
    // The agent's own message shows that it received the input and is now hanging.
    env.wait_for(&stream, 0, |e| matches!(e, Event::ItemCompleted { item } if matches!(item.body, ItemBody::AgentMessage { .. }))).await;
    assert_eq!(env.engine.running_processes(), 1);

    let asked = std::time::Instant::now();
    let r = env
        .call::<spec::TurnInterrupt>(TurnInterruptParams {
            client_request_id: crid(),
            thread_id: thread.id.clone(),
        })
        .await
        .unwrap();
    assert!(r.interrupted);
    let (turn, events) = env.wait_turn_done(&thread.id, 0).await;
    assert!(
        asked.elapsed() >= grace,
        "the agent had interrupt_grace to comply"
    );
    assert_eq!(turn.status, TurnStatus::Interrupted);
    assert_eq!(turn.error.as_ref().map(|e| e.kind.as_str()), Some("forced"));
    let after = events.last().unwrap().seq;
    env.wait_for(
        &stream,
        after,
        |e| matches!(e, Event::ThreadUpdated { thread } if thread.status == ThreadStatus::Idle),
    )
    .await;
    assert_eq!(
        env.engine.running_processes(),
        0,
        "the agent's process is gone"
    );
    let read = env.read(&thread.id).await;
    assert!(
        read.thread.last_error.is_none(),
        "a forced stop the user asked for is not an agent failure"
    );
    assert!(
        read.items
            .iter()
            .all(|i| i.status != ItemStatus::InProgress)
    );

    // The next turn works, on a new process that resumes the session.
    env.send(&thread.id, "after the stop", Delivery::Auto).await;
    let events = env
        .wait_for(
            &stream,
            after,
            |e| matches!(e, Event::TurnCompleted { turn } if turn.index == 1),
        )
        .await;
    let done = events
        .iter()
        .find_map(|e| match &e.event {
            Event::TurnCompleted { turn } if turn.index == 1 => Some(turn.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(done.status, TurnStatus::Completed);
    assert!(agent_text(&env.read(&thread.id).await.items).contains("echo: after the stop"));
    env.engine.shutdown(false).await;
    assert_eq!(env.engine.running_processes(), 0);
}

// ----- pairing and maintenance -------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn pairing_attempts_are_rate_limited() {
    let env = env_with(|p| p.pairing_attempts_per_window = 3).await;
    for _ in 0..3 {
        assert_eq!(
            env.engine
                .pair("WRNG-CODE", "phone", "android")
                .await
                .unwrap_err(),
            PairError::InvalidCode
        );
    }
    let (code, _) = env.engine.create_pairing_code().await.unwrap();
    assert_eq!(
        env.engine
            .pair(&code, "phone", "android")
            .await
            .unwrap_err(),
        PairError::RateLimited,
        "once the limit is reached even a valid code is refused"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_pairing_window_and_the_device_name_limits_are_policy_values() {
    let window = Duration::from_secs(1);
    let env = env_with(|p| {
        p.pairing_attempts_per_window = 1;
        p.pairing_rate_window = window;
        p.device_name_chars = 5;
        p.device_platform_chars = 3;
    })
    .await;
    let (code, _) = env.engine.create_pairing_code().await.unwrap();
    assert_eq!(
        env.engine.pair("WRNG-CODE", "x", "y").await.unwrap_err(),
        PairError::InvalidCode
    );
    assert_eq!(
        env.engine.pair(&code, "x", "y").await.unwrap_err(),
        PairError::RateLimited
    );
    tokio::time::sleep(window + Duration::from_millis(100)).await;
    let paired = env
        .engine
        .pair(&code, "  My Pixel 9 Pro  ", "android")
        .await
        .unwrap();
    let devices = env.engine.list_devices(None).await.unwrap();
    let device = devices.iter().find(|d| d.id == paired.device_id).unwrap();
    assert_eq!(device.name, "My Pi");
    assert_eq!(device.platform.as_deref(), Some("and"));
}

#[tokio::test(flavor = "multi_thread")]
async fn maintenance_expires_idempotency_records_and_pairing_codes() {
    let env = env_with(|p| {
        p.idempotency_ttl = Duration::from_millis(100);
        p.pairing_code_ttl = Duration::from_millis(100);
    })
    .await;
    let (a, b) = (env.root.join("a"), env.root.join("b"));
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    let open = |path: &Path| ProjectOpenParams {
        client_request_id: "same-key".into(),
        path: path.display().to_string(),
        name: None,
    };
    env.call::<spec::ProjectOpen>(open(&a)).await.unwrap();
    let reused = env.call::<spec::ProjectOpen>(open(&b)).await.unwrap_err();
    assert_eq!(reused.kind(), Some(ErrorKind::IdempotencyKeyReused));
    let (code, _) = env.engine.create_pairing_code().await.unwrap();

    tokio::time::sleep(Duration::from_millis(250)).await;
    let report = env.engine.run_maintenance().await.unwrap();
    assert!(report.idempotency_records >= 1, "{report:?}");
    assert_eq!(report.pairing_codes, 1, "{report:?}");
    // The expired record no longer binds the key; the expired code is gone.
    let reopened = env.call::<spec::ProjectOpen>(open(&b)).await.unwrap();
    assert!(reopened.project.path.ends_with('b'));
    assert_eq!(
        env.engine
            .pair(&code, "phone", "android")
            .await
            .unwrap_err(),
        PairError::InvalidCode
    );
    let again = env.engine.run_maintenance().await.unwrap();
    assert_eq!(
        (again.pairing_codes, again.idempotency_records),
        (0, 0),
        "nothing is left to expire"
    );
}

// ----- retention ---------------------------------------------------------------------------------

impl Env {
    async fn upload(&self, content: &[u8]) -> BlobId {
        self.engine
            .put_blob(content.to_vec(), "image/png")
            .await
            .unwrap()
            .blob_id
    }

    async fn blob_exists(&self, id: &BlobId) -> bool {
        self.engine.blob(id).await.unwrap().is_some()
    }

    async fn send_parts(
        &self,
        thread: &ThreadId,
        input: Vec<InputPart>,
        delivery: Delivery,
    ) -> TurnStartResult {
        self.call::<spec::TurnStart>(TurnStartParams {
            client_request_id: crid(),
            thread_id: thread.clone(),
            input,
            delivery,
        })
        .await
        .unwrap()
    }

    async fn stop(&self, thread: &ThreadId) {
        self.call::<spec::ThreadStop>(ThreadStopParams {
            client_request_id: crid(),
            thread_id: thread.clone(),
        })
        .await
        .unwrap();
    }

    /// The whole stored log of `stream`.
    async fn log(&self, stream: &str) -> Vec<EventEnvelope> {
        let mut out = Vec::new();
        let mut cursor = 0;
        loop {
            let batch = self
                .engine
                .read_batch(stream.to_owned(), cursor)
                .await
                .unwrap();
            if batch.events.is_empty() {
                return out;
            }
            cursor = batch.last_seq;
            out.extend(batch.events);
        }
    }
}

fn text(t: &str) -> InputPart {
    InputPart::Text { text: t.into() }
}

fn output_blob(items: &[Item]) -> Option<BlobId> {
    items.iter().find_map(|i| match &i.body {
        ItemBody::CommandExecution { output_blob_id, .. } => output_blob_id.clone(),
        _ => None,
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn blobs_nothing_refers_to_are_deleted_after_the_grace_period() {
    let grace = Duration::from_millis(300);
    let env = env_with(|p| {
        p.unreferenced_blob_grace = grace;
        p.max_inline_output_bytes = 1024;
    })
    .await;
    let project = env.project("p", false).await;
    let thread = env.thread(&project).await;
    let unused = env.upload(b"never sent").await;
    let attached = env.upload(b"sent with a message").await;
    let queued = env.upload(b"waiting in the queue").await;

    // An image sent with a message, and a command output long enough to be spilled.
    env.send_parts(
        &thread.id,
        vec![
            text("@bigoutput 5000"),
            InputPart::Image {
                blob_id: attached.clone(),
            },
        ],
        Delivery::Auto,
    )
    .await;
    env.wait_turn_done(&thread.id, 0).await;
    let spilled =
        output_blob(&env.read(&thread.id).await.items).expect("the output was spilled to a blob");
    // An image in a queued input (a turn is running).
    env.send(&thread.id, "@sleep 600000", Delivery::Auto).await;
    let queued_entry = env
        .send_parts(
            &thread.id,
            vec![
                text("later"),
                InputPart::Image {
                    blob_id: queued.clone(),
                },
            ],
            Delivery::Queue,
        )
        .await
        .queued_id
        .unwrap();

    tokio::time::sleep(grace + Duration::from_millis(100)).await;
    let report = env.engine.run_maintenance().await.unwrap();
    assert_eq!(report.blobs, 1, "only the unsent upload goes: {report:?}");
    assert!(!env.blob_exists(&unused).await);
    for kept in [&attached, &spilled, &queued] {
        assert!(env.blob_exists(kept).await, "{kept} is referred to");
    }

    // Removing the queued input releases its image; it goes once its grace period is over.
    let removed = env
        .call::<spec::QueueRemove>(QueueRemoveParams {
            client_request_id: crid(),
            thread_id: thread.id.clone(),
            queued_id: queued_entry,
        })
        .await
        .unwrap();
    assert!(removed.removed);
    assert_eq!(
        env.engine.run_maintenance().await.unwrap().blobs,
        0,
        "the grace period starts when the last reference goes"
    );
    assert!(env.blob_exists(&queued).await);
    tokio::time::sleep(grace + Duration::from_millis(100)).await;
    assert_eq!(env.engine.run_maintenance().await.unwrap().blobs, 1);
    assert!(!env.blob_exists(&queued).await);

    // Content-addressed: uploading deleted content again stores it again under the same id.
    assert_eq!(env.upload(b"never sent").await, unused);
    assert!(env.blob_exists(&unused).await);
    env.call::<spec::TurnInterrupt>(TurnInterruptParams {
        client_request_id: crid(),
        thread_id: thread.id.clone(),
    })
    .await
    .unwrap();
    env.engine.shutdown(false).await;
}

fn git_output(dir: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[tokio::test(flavor = "multi_thread")]
async fn removing_a_project_purges_its_threads_and_what_they_left_outside_the_database() {
    if !git_available() {
        eprintln!("git not installed; skipping");
        return;
    }
    let grace = Duration::from_millis(200);
    let env = env_with(|p| p.unreferenced_blob_grace = grace).await;
    let project = env.project("repo", true).await;
    let repo = PathBuf::from(&project.path);
    let local = env.thread(&project).await;
    let image = env.upload(b"a screenshot").await;
    env.send_parts(
        &local.id,
        vec![
            text("@write a.txt one\n@bigoutput 100000"),
            InputPart::Image {
                blob_id: image.clone(),
            },
        ],
        Delivery::Auto,
    )
    .await;
    env.wait_turn_done(&local.id, 0).await;
    let spilled = output_blob(&env.read(&local.id).await.items).expect("a spilled output");
    let wt = env
        .call::<spec::ThreadCreate>(ThreadCreateParams {
            client_request_id: crid(),
            project_id: project.id.clone(),
            harness_id: "fake".into(),
            settings: None,
            workspace: Some(WorkspaceSpec::Worktree {
                base_ref: None,
                branch: None,
            }),
            title: None,
            input: Some(vec![text("@write wt.txt from the worktree")]),
        })
        .await
        .unwrap();
    let Workspace::Worktree { path: worktree, .. } = wt.thread.workspace.clone() else {
        panic!("expected a worktree")
    };
    env.wait_turn_done(&wt.thread.id, 0).await;
    let refs = git_output(&repo, &["for-each-ref", "--format=%(refname)", "refs/aas/"]);
    for thread in [&local.id, &wt.thread.id] {
        assert!(
            refs.contains(thread.as_str()),
            "snapshots of {thread} are kept reachable: {refs}"
        );
    }
    env.stop(&local.id).await;
    env.stop(&wt.thread.id).await;

    // The agent left an uncommitted file in the worktree: removal refuses, nothing is removed.
    let remove = || ProjectRemoveParams {
        client_request_id: crid(),
        project_id: project.id.clone(),
    };
    let refused = env.call::<spec::ProjectRemove>(remove()).await.unwrap_err();
    assert_eq!(refused.kind(), Some(ErrorKind::InvalidState), "{refused:?}");
    assert!(refused.message.contains("uncommitted"), "{refused:?}");
    assert_eq!(
        env.read(&wt.thread.id).await.turns.len(),
        1,
        "nothing was removed"
    );
    let worktree_dir = PathBuf::from(&worktree);
    git_output(&worktree_dir, &["add", "-A"]);
    git_output(
        &worktree_dir,
        &[
            "-c",
            "user.email=t@example.com",
            "-c",
            "user.name=t",
            "commit",
            "-q",
            "-m",
            "keep it",
        ],
    );
    let head_before = env
        .engine
        .read_batch(WORKSPACE_STREAM.into(), 0)
        .await
        .unwrap()
        .head;

    env.call::<spec::ProjectRemove>(remove()).await.unwrap();

    for thread in [&local.id, &wt.thread.id] {
        let read = env
            .call::<spec::ThreadRead>(ThreadReadParams {
                thread_id: thread.clone(),
                before_turn_index: None,
                limit_turns: None,
            })
            .await
            .unwrap_err();
        assert_eq!(read.kind(), Some(ErrorKind::NotFound));
        assert_eq!(
            env.engine
                .stream_head(&thread_stream(thread))
                .await
                .unwrap(),
            None,
            "the thread's stream is gone"
        );
        assert!(env.log(&thread_stream(thread)).await.is_empty());
    }
    assert!(
        git_output(&repo, &["for-each-ref", "refs/aas/"])
            .trim()
            .is_empty(),
        "the snapshot refs are gone"
    );
    assert!(
        !worktree_dir.exists(),
        "the worktree the daemon created is removed"
    );
    assert!(
        !git_output(&repo, &["worktree", "list"]).contains("worktrees"),
        "git forgot it too"
    );
    assert!(
        repo.join("a.txt").exists(),
        "the project's own files are never touched"
    );
    // Offline clients still learn about the removals; nothing else about the threads is left.
    let workspace = env.log(WORKSPACE_STREAM).await;
    for thread in [&local.id, &wt.thread.id] {
        let about: Vec<&str> = workspace
            .iter()
            .filter(|e| match &e.event {
                Event::ThreadUpserted { thread: t } => &t.id == thread,
                Event::ThreadRemoved { thread_id } => thread_id == thread,
                Event::InteractionPending { interaction } => &interaction.thread_id == thread,
                Event::InteractionClosed { thread_id, .. } => thread_id == thread,
                _ => false,
            })
            .map(|e| e.event.type_name())
            .collect();
        assert_eq!(
            about,
            vec!["thread/removed"],
            "workspace events about {thread}"
        );
    }
    assert!(workspace.iter().any(|e| e.seq > head_before
        && matches!(&e.event, Event::ProjectRemoved { project_id } if project_id == &project.id)));

    // Its blobs lose their references and go after the grace period; the space is returned.
    tokio::time::sleep(grace + Duration::from_millis(100)).await;
    let report = env.engine.run_maintenance().await.unwrap();
    assert_eq!(report.blobs, 2, "{report:?}");
    assert!(!env.blob_exists(&image).await && !env.blob_exists(&spilled).await);
    assert!(
        report.vacuumed_pages > 0,
        "the purged rows' pages were returned: {report:?}"
    );
    assert_eq!(
        env.engine.database_pages().await.unwrap().1,
        0,
        "no free page is left"
    );
    assert_eq!(
        (report.cleanup_done, report.cleanup_pending),
        (0, 0),
        "the cleanup was done during the removal"
    );

    // Opening the folder again brings the project back without its old threads.
    let back = env.project("repo", false).await;
    assert_eq!(back.id, project.id);
    let threads = env
        .call::<spec::ThreadList>(ThreadListParams {
            project_id: Some(project.id.clone()),
            include_archived: true,
            limit: None,
            before: None,
        })
        .await
        .unwrap();
    assert!(threads.threads.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refused_first_turn_leaves_no_worktree_behind() {
    if !git_available() {
        eprintln!("git not installed; skipping");
        return;
    }
    let env = env().await;
    let project = env.project("repo", true).await;
    let repo = PathBuf::from(&project.path);
    let create = |input: Vec<InputPart>| ThreadCreateParams {
        client_request_id: crid(),
        project_id: project.id.clone(),
        harness_id: "fake".into(),
        settings: None,
        workspace: Some(WorkspaceSpec::Worktree {
            base_ref: None,
            branch: None,
        }),
        title: None,
        input: Some(input),
    };
    // An image whose blob is gone (collected), and a mention outside the thread's folder.
    let missing = BlobId::from_sha256_hex(&"ab".repeat(32));
    let err = env
        .call::<spec::ThreadCreate>(create(vec![
            text("look"),
            InputPart::Image { blob_id: missing },
        ]))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), Some(ErrorKind::NotFound), "{err:?}");
    let err = env
        .call::<spec::ThreadCreate>(create(vec![InputPart::Mention {
            path: "../secret".into(),
        }]))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), Some(ErrorKind::InvalidParams), "{err:?}");
    let worktrees = env.data.join("worktrees").join(project.id.as_str());
    let left: Vec<_> = std::fs::read_dir(&worktrees)
        .map(|d| d.flatten().map(|e| e.path()).collect())
        .unwrap_or_default();
    assert!(left.is_empty(), "no worktree folder: {left:?}");
    assert!(
        git_output(&repo, &["branch", "--list", "aas/*"])
            .trim()
            .is_empty(),
        "no branch"
    );
    assert!(
        !git_output(&repo, &["worktree", "list"]).contains("worktrees"),
        "git knows no worktree"
    );
    let threads = env
        .call::<spec::ThreadList>(ThreadListParams {
            project_id: Some(project.id.clone()),
            include_archived: true,
            limit: None,
            before: None,
        })
        .await
        .unwrap();
    assert!(threads.threads.is_empty());
    // A valid first turn still gets its worktree.
    let created = env
        .call::<spec::ThreadCreate>(create(vec![text("hello")]))
        .await
        .unwrap();
    let Workspace::Worktree { path, branch, .. } = created.thread.workspace.clone() else {
        panic!("expected a worktree")
    };
    assert!(Path::new(&path).is_dir());
    assert!(git_output(&repo, &["branch", "--list", &branch]).contains(&branch));
    env.wait_turn_done(&created.thread.id, 0).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_base_ref_that_is_an_option_or_names_no_commit_is_refused_before_anything_is_created() {
    if !git_available() {
        eprintln!("git not installed; skipping");
        return;
    }
    let env = env().await;
    let project = env.project("repo", true).await;
    let repo = PathBuf::from(&project.path);
    let create = |base_ref: Option<&str>, branch: Option<&str>| ThreadCreateParams {
        client_request_id: crid(),
        project_id: project.id.clone(),
        harness_id: "fake".into(),
        settings: None,
        workspace: Some(WorkspaceSpec::Worktree {
            base_ref: base_ref.map(str::to_owned),
            branch: branch.map(str::to_owned),
        }),
        title: None,
        input: None,
    };
    for (base, branch) in [
        (Some("-x"), None),
        (Some("--orphan"), None),
        (Some(""), None),
        (Some("no-such-branch"), None),
        // A tree is not a commit.
        (Some("HEAD^{tree}"), None),
        (None, Some("-b")),
    ] {
        let err = env
            .call::<spec::ThreadCreate>(create(base, branch))
            .await
            .unwrap_err();
        assert_eq!(
            err.kind(),
            Some(ErrorKind::InvalidParams),
            "{base:?} {branch:?}: {err:?}"
        );
    }
    let worktrees = env.data.join("worktrees").join(project.id.as_str());
    let left: Vec<_> = std::fs::read_dir(&worktrees)
        .map(|d| d.flatten().map(|e| e.path()).collect())
        .unwrap_or_default();
    assert!(left.is_empty(), "no worktree folder: {left:?}");
    assert!(
        git_output(&repo, &["branch", "--list", "aas/*"])
            .trim()
            .is_empty(),
        "no branch"
    );
    // A name of a commit works, and is recorded as given.
    let head = git_output(&repo, &["rev-parse", "HEAD"]).trim().to_owned();
    let created = env
        .call::<spec::ThreadCreate>(create(Some(&head[..12]), None))
        .await
        .unwrap();
    let Workspace::Worktree { path, base_ref, .. } = created.thread.workspace.clone() else {
        panic!("expected a worktree")
    };
    assert_eq!(base_ref, head[..12]);
    assert!(Path::new(&path).join("README.md").exists());
}

/// Renames a folder, waiting while another program (an indexer, a virus scanner) briefly
/// holds a file in it open.
fn rename_folder(from: &Path, to: &Path) {
    let deadline = std::time::Instant::now() + WAIT;
    loop {
        match std::fs::rename(from, to) {
            Ok(()) => return,
            Err(e) if std::time::Instant::now() < deadline => {
                eprintln!("renaming {} failed ({e}); retrying", from.display());
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => panic!("renaming {}: {e}", from.display()),
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_worktree_whose_repository_cannot_be_reached_is_kept_until_it_can() {
    if !git_available() {
        eprintln!("git not installed; skipping");
        return;
    }
    let env = env().await;
    let project = env.project("repo", true).await;
    let repo = PathBuf::from(&project.path);
    let wt = env
        .call::<spec::ThreadCreate>(ThreadCreateParams {
            client_request_id: crid(),
            project_id: project.id.clone(),
            harness_id: "fake".into(),
            settings: None,
            workspace: Some(WorkspaceSpec::Worktree {
                base_ref: None,
                branch: None,
            }),
            title: None,
            input: Some(vec![text("@write work.txt the only copy of this work")]),
        })
        .await
        .unwrap();
    let Workspace::Worktree { path: worktree, .. } = wt.thread.workspace.clone() else {
        panic!("expected a worktree")
    };
    let worktree = PathBuf::from(worktree);
    env.wait_turn_done(&wt.thread.id, 0).await;
    env.stop(&wt.thread.id).await;
    assert!(worktree.join("work.txt").exists());
    // The drive the repository lives on is not connected right now.
    let away = env.root.join("repo-unplugged");
    rename_folder(&repo, &away);
    env.call::<spec::ProjectRemove>(ProjectRemoveParams {
        client_request_id: crid(),
        project_id: project.id.clone(),
    })
    .await
    .unwrap();
    assert!(
        worktree.join("work.txt").exists(),
        "the uncommitted work is not deleted on a guess"
    );
    let report = env.engine.run_maintenance().await.unwrap();
    assert_eq!(
        (report.cleanup_done, report.cleanup_pending),
        (0, 2),
        "both jobs wait for the repository: {report:?}"
    );
    assert!(worktree.join("work.txt").exists());
    // The drive is back: git checks the worktree, and keeps it because of the uncommitted file.
    rename_folder(&away, &repo);
    let report = env.engine.run_maintenance().await.unwrap();
    assert_eq!(
        (report.cleanup_done, report.cleanup_pending),
        (1, 1),
        "the refs go, the dirty worktree stays: {report:?}"
    );
    assert!(worktree.join("work.txt").exists());
    assert!(
        git_output(&repo, &["for-each-ref", "refs/aas/"])
            .trim()
            .is_empty(),
        "the snapshot refs came back and are deleted now"
    );
    // Once the work is committed, the worktree goes as well.
    git_output(&worktree, &["add", "-A"]);
    git_output(
        &worktree,
        &[
            "-c",
            "user.email=t@example.com",
            "-c",
            "user.name=t",
            "commit",
            "-q",
            "-m",
            "keep it",
        ],
    );
    let report = env.engine.run_maintenance().await.unwrap();
    assert_eq!(
        (report.cleanup_done, report.cleanup_pending),
        (1, 0),
        "{report:?}"
    );
    assert!(!worktree.exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn compaction_keeps_the_latest_state_of_every_entity() {
    let env = env_with(|p| {
        p.delta_retention = Duration::ZERO;
        p.superseded_event_retention = Duration::ZERO;
        p.native_event_retention = Duration::ZERO;
    })
    .await;
    let project = env.project("p", false).await;
    let thread = env.thread(&project).await;
    let stream = thread_stream(&thread.id);
    env.send(
        &thread.id,
        "@stream 20\n@plan\n@context 100 1000",
        Delivery::Auto,
    )
    .await;
    env.wait_turn_done(&thread.id, 0).await;
    for title in ["first title", "second title"] {
        env.call::<spec::ThreadUpdate>(ThreadUpdateParams {
            client_request_id: crid(),
            thread_id: thread.id.clone(),
            title: Some(title.into()),
            settings: None,
            pinned: None,
            modes: None,
        })
        .await
        .unwrap();
    }
    env.stop(&thread.id).await;

    /// What a client ends up with after applying `events` in order.
    #[derive(Debug, PartialEq)]
    struct Applied {
        threads: std::collections::BTreeMap<String, Thread>,
        turns: std::collections::BTreeMap<String, Turn>,
        items: std::collections::BTreeMap<String, Item>,
        commands_signals: bool,
    }
    fn apply(events: &[EventEnvelope]) -> Applied {
        let mut a = Applied {
            threads: Default::default(),
            turns: Default::default(),
            items: Default::default(),
            commands_signals: false,
        };
        for e in events {
            match &e.event {
                Event::ThreadUpdated { thread } | Event::ThreadUpserted { thread } => {
                    a.threads.insert(thread.id.to_string(), thread.clone());
                }
                Event::TurnStarted { turn } | Event::TurnCompleted { turn } => {
                    a.turns.insert(turn.id.to_string(), turn.clone());
                }
                Event::TurnUsageUpdated { turn_id, usage } => {
                    if let Some(t) = a.turns.get_mut(turn_id.as_str()) {
                        t.usage = Some(*usage);
                    }
                }
                Event::TurnDiffUpdated { turn_id, diff } => {
                    if let Some(t) = a.turns.get_mut(turn_id.as_str()) {
                        t.diff = Some(*diff);
                    }
                }
                Event::ItemStarted { item }
                | Event::ItemUpdated { item }
                | Event::ItemCompleted { item } => {
                    a.items.insert(item.id.to_string(), item.clone());
                }
                Event::ItemDelta {
                    item_id,
                    field,
                    text,
                } => {
                    if let Some(i) = a.items.get_mut(item_id.as_str()) {
                        i.body.append(*field, text);
                    }
                }
                Event::CommandsChanged {} => a.commands_signals = true,
                _ => {}
            }
        }
        a
    }
    let before_thread = env.log(&stream).await;
    let before_workspace = env.log(WORKSPACE_STREAM).await;
    tokio::time::sleep(Duration::from_millis(20)).await;
    let report = env.engine.run_maintenance().await.unwrap();
    assert!(report.deltas > 0 && report.superseded > 0, "{report:?}");
    let after_thread = env.log(&stream).await;
    let after_workspace = env.log(WORKSPACE_STREAM).await;
    assert!(after_thread.len() < before_thread.len());
    assert_eq!(
        apply(&after_thread),
        apply(&before_thread),
        "a client replaying the compacted log ends in the same state"
    );
    assert_eq!(apply(&after_workspace), apply(&before_workspace));
    let count = |events: &[EventEnvelope], name: &str| {
        events
            .iter()
            .filter(|e| e.event.type_name() == name)
            .count()
    };
    assert_eq!(
        count(&after_thread, "thread/updated"),
        1,
        "only the latest summary is left"
    );
    assert_eq!(count(&after_workspace, "thread/upserted"), 1);
    assert_eq!(
        count(&after_thread, "turn/started"),
        1,
        "a turn's start is never compacted"
    );
    assert_eq!(
        count(&after_thread, "item/started"),
        count(&before_thread, "item/started")
    );
    assert_eq!(
        count(&after_thread, "item/completed"),
        count(&before_thread, "item/completed")
    );
    assert_eq!(
        count(&after_thread, "item/delta") + count(&after_thread, "item/updated"),
        0
    );
    assert_eq!(
        count(&after_thread, "turn/usageUpdated"),
        0,
        "turn/completed carries the final usage"
    );
    assert!(count(&after_thread, "native") <= count(&before_thread, "native"));
    let again = env.engine.run_maintenance().await.unwrap();
    assert_eq!(
        (again.deltas, again.superseded, again.native),
        (0, 0, 0),
        "compaction is idempotent"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn finished_operations_are_forgotten_after_their_retention() {
    if !git_available() {
        eprintln!("git not installed; skipping");
        return;
    }
    let env = env_with(|p| p.finished_operation_retention = Duration::from_millis(100)).await;
    let origin = env.project("origin", true).await;
    let url = format!("file:///{}", origin.path.replace('\\', "/"));
    let op = env.clone_into("copy", &url).await;
    let (_, finished) = env.wait_operation_end(&op.id, 0).await;
    assert_eq!(finished.status, OperationStatus::Succeeded);
    assert_eq!(
        env.engine.run_maintenance().await.unwrap().operations,
        0,
        "a recent operation is kept"
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(env.engine.run_maintenance().await.unwrap().operations, 1);
    let listed = env.call::<spec::OperationList>(Empty {}).await.unwrap();
    assert!(listed.operations.iter().all(|o| o.id != op.id));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_restart_removes_files_a_previous_run_left_behind() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let root = dir.path().join("projects");
    std::fs::create_dir_all(&root).unwrap();
    let root = dunce::canonicalize(&root).unwrap();
    let engine = start_engine(&data, &root, test_policy()).await;
    let kept = engine
        .put_blob(b"recorded".to_vec(), "image/png")
        .await
        .unwrap()
        .blob_id;
    engine.shutdown(false).await;
    engine.close().await.unwrap();
    drop(engine);
    // What a crash can leave: a spill in progress, a snapshot index, a blob file whose
    // transaction never committed.
    std::fs::write(data.join("tmp").join("spill-crashed"), b"partial").unwrap();
    std::fs::write(data.join("tmp").join("snapshot-crashed.index"), b"index").unwrap();
    let hex = "ab".repeat(32);
    let stray = data.join("blobs").join(&hex[..2]).join(&hex);
    std::fs::create_dir_all(stray.parent().unwrap()).unwrap();
    std::fs::write(&stray, b"never recorded").unwrap();

    let engine = start_engine(&data, &root, test_policy()).await;
    assert_eq!(
        std::fs::read_dir(data.join("tmp")).unwrap().count(),
        0,
        "temporary files are removed"
    );
    assert!(!stray.exists(), "a blob file without a record is removed");
    assert!(
        engine.blob(&kept).await.unwrap().is_some(),
        "recorded blobs stay"
    );
    engine.shutdown(false).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn threads_an_earlier_version_only_marked_as_removed_are_purged_at_startup() {
    let env = env().await;
    let project = env.project("p", false).await;
    let thread = env.thread(&project).await;
    env.send(&thread.id, "hello", Delivery::Auto).await;
    env.wait_turn_done(&thread.id, 0).await;
    env.engine.shutdown(false).await;
    env.engine.close().await.unwrap();
    {
        // What an older version left: the thread marked removed, everything else in place.
        let conn = rusqlite::Connection::open(env.data.join("aas.db")).unwrap();
        conn.execute(
            "UPDATE threads SET removed = 1 WHERE id = ?1",
            [thread.id.as_str()],
        )
        .unwrap();
    }
    let engine = start_engine(&env.data, &env.root, env.policy.clone()).await;
    assert_eq!(
        engine
            .stream_head(&thread_stream(&thread.id))
            .await
            .unwrap(),
        None
    );
    assert!(
        engine
            .read_batch(thread_stream(&thread.id), 0)
            .await
            .unwrap()
            .events
            .is_empty(),
        "its events are gone"
    );
    let workspace = engine.read_batch(WORKSPACE_STREAM.into(), 0).await.unwrap();
    assert!(
        !workspace
            .events
            .iter()
            .any(|e| matches!(&e.event, Event::ThreadUpserted { thread: t } if t.id == thread.id)),
        "nothing about it is left in the workspace stream"
    );
    engine.shutdown(false).await;
}

// ----- request paths without a test of their own -------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn project_settings_archiving_and_the_workspace_snapshot() {
    let env = env().await;
    let project = env.project("p", false).await;
    let other = env.project("q", false).await;
    let got = env
        .call::<spec::ProjectGet>(ProjectGetParams {
            project_id: project.id.clone(),
        })
        .await
        .unwrap();
    assert_eq!(got.project.id, project.id);

    // Defaults name a configured harness only; they are then used by thread/create.
    let update = |defaults: ProjectDefaults| ProjectUpdateParams {
        client_request_id: crid(),
        project_id: project.id.clone(),
        name: Some("  Renamed  ".into()),
        defaults: Some(defaults),
        harness_trust: None,
    };
    let bad = env
        .call::<spec::ProjectUpdate>(update(ProjectDefaults {
            harness_id: Some("nope".into()),
            ..Default::default()
        }))
        .await
        .unwrap_err();
    assert_eq!(bad.kind(), Some(ErrorKind::InvalidParams));
    let updated = env
        .call::<spec::ProjectUpdate>(update(ProjectDefaults {
            harness_id: Some("fake".into()),
            model: Some("fake-slow".into()),
            effort: Some("high".into()),
            permission_mode: Some("auto".into()),
        }))
        .await
        .unwrap();
    assert_eq!(updated.project.name, "Renamed");
    let thread = env.thread(&project).await;
    assert_eq!(
        thread.settings.model.as_deref(),
        Some("fake-slow"),
        "the project's defaults apply"
    );
    assert_eq!(thread.settings.effort.as_deref(), Some("high"));
    assert_eq!(thread.settings.permission_mode.as_deref(), Some("auto"));

    // Archived projects leave the default listing and the snapshot; opening brings them back.
    let archived = env
        .call::<spec::ProjectArchive>(ProjectArchiveParams {
            client_request_id: crid(),
            project_id: other.id.clone(),
            archived: true,
        })
        .await
        .unwrap();
    assert!(archived.project.archived);
    let listed = env
        .call::<spec::ProjectList>(ProjectListParams {
            include_archived: false,
        })
        .await
        .unwrap();
    assert!(listed.projects.iter().all(|p| p.id != other.id));
    let all = env
        .call::<spec::ProjectList>(ProjectListParams {
            include_archived: true,
        })
        .await
        .unwrap();
    assert!(all.projects.iter().any(|p| p.id == other.id));
    let snapshot = env.call::<spec::WorkspaceSnapshot>(Empty {}).await.unwrap();
    assert_eq!(
        snapshot
            .projects
            .iter()
            .map(|p| p.id.clone())
            .collect::<Vec<_>>(),
        vec![project.id.clone()]
    );
    assert_eq!(
        snapshot
            .threads
            .iter()
            .map(|t| t.id.clone())
            .collect::<Vec<_>>(),
        vec![thread.id.clone()]
    );
    assert_eq!(snapshot.harnesses.len(), 1);
    assert_eq!(
        snapshot.head,
        env.engine
            .read_batch(WORKSPACE_STREAM.into(), 0)
            .await
            .unwrap()
            .head,
        "the head matches its content"
    );
    let reopened = env.project("q", false).await;
    assert!(!reopened.archived);
    let missing = env
        .call::<spec::ProjectGet>(ProjectGetParams {
            project_id: ProjectId::from("prj_missing"),
        })
        .await
        .unwrap_err();
    assert_eq!(missing.kind(), Some(ErrorKind::NotFound));
}

#[tokio::test(flavor = "multi_thread")]
async fn devices_harnesses_folders_and_interactions_through_requests() {
    let env = env().await;
    // Devices: the caller is marked; revoking an unknown device is notFound.
    let (code, _) = env.engine.create_pairing_code().await.unwrap();
    let paired = env.engine.pair(&code, "Tablet", "android").await.unwrap();
    let as_tablet = RequestCtx {
        device_id: paired.device_id.clone(),
    };
    let listed: DeviceListResult = serde_json::from_value(
        env.engine
            .handle(
                &as_tablet,
                ClientRequest::parse("device/list", Some(json!({}))).unwrap(),
            )
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(listed.devices.len(), 1);
    assert!(listed.devices[0].current);
    let unknown = env
        .call::<spec::DeviceRevoke>(DeviceRevokeParams {
            client_request_id: crid(),
            device_id: DeviceId::from("dev_unknown"),
        })
        .await
        .unwrap_err();
    assert_eq!(unknown.kind(), Some(ErrorKind::NotFound));
    env.call::<spec::DeviceRevoke>(DeviceRevokeParams {
        client_request_id: crid(),
        device_id: paired.device_id.clone(),
    })
    .await
    .unwrap();
    assert!(
        env.call::<spec::DeviceList>(Empty {})
            .await
            .unwrap()
            .devices
            .is_empty()
    );

    // Harnesses: a refresh publishes the result; an unknown id is notFound.
    let before = env
        .engine
        .read_batch(WORKSPACE_STREAM.into(), 0)
        .await
        .unwrap()
        .head;
    let refreshed = env
        .call::<spec::HarnessRefresh>(HarnessRefreshParams {
            harness_id: Some("fake".into()),
        })
        .await
        .unwrap();
    assert_eq!(refreshed.harnesses.len(), 1);
    assert!(refreshed.harnesses[0].available);
    assert!(
        refreshed.harnesses[0].capabilities.images,
        "the fake harness takes images"
    );
    env.wait_for(WORKSPACE_STREAM, before, |e| {
        matches!(e, Event::HarnessUpdated { .. })
    })
    .await;
    let unknown = env
        .call::<spec::HarnessRefresh>(HarnessRefreshParams {
            harness_id: Some("nope".into()),
        })
        .await
        .unwrap_err();
    assert_eq!(unknown.kind(), Some(ErrorKind::NotFound));
    assert_eq!(
        env.call::<spec::HarnessList>(Empty {})
            .await
            .unwrap()
            .harnesses
            .len(),
        1
    );

    // Folders: creating an existing empty folder again succeeds (a resend), a used one does not.
    let target = env.root.join("made");
    let mkdir = || FsMkdirParams {
        client_request_id: crid(),
        path: target.display().to_string(),
    };
    env.call::<spec::FsMkdir>(mkdir()).await.unwrap();
    env.call::<spec::FsMkdir>(mkdir()).await.unwrap();
    std::fs::write(target.join("file"), b"x").unwrap();
    assert_eq!(
        env.call::<spec::FsMkdir>(mkdir()).await.unwrap_err().kind(),
        Some(ErrorKind::AlreadyExists)
    );

    // Native sessions: the fake harness has none to offer.
    let project = env.project("p", false).await;
    let native = env
        .call::<spec::NativeList>(NativeListParams {
            project_id: project.id.clone(),
            harness_id: "fake".into(),
        })
        .await
        .unwrap_err();
    assert_eq!(native.kind(), Some(ErrorKind::CapabilityUnsupported));

    // Interactions: pending ones are listed, filtered by status.
    let thread = env.thread(&project).await;
    env.send(&thread.id, "@approve cargo test", Delivery::Auto)
        .await;
    env.wait_for(&thread_stream(&thread.id), 0, |e| {
        matches!(e, Event::InteractionRequested { .. })
    })
    .await;
    let pending = env
        .call::<spec::InteractionList>(InteractionListParams { status: None })
        .await
        .unwrap();
    assert_eq!(pending.interactions.len(), 1);
    let resolved = env
        .call::<spec::InteractionList>(InteractionListParams {
            status: Some(InteractionStatus::Resolved),
        })
        .await
        .unwrap();
    assert!(resolved.interactions.is_empty());
    env.call::<spec::TurnInterrupt>(TurnInterruptParams {
        client_request_id: crid(),
        thread_id: thread.id.clone(),
    })
    .await
    .unwrap();
    env.wait_turn_done(&thread.id, 0).await;
    assert!(
        env.call::<spec::InteractionList>(InteractionListParams { status: None })
            .await
            .unwrap()
            .interactions
            .is_empty()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn page_sizes_follow_the_policy() {
    let env = env_with(|p| {
        p.thread_list_default_limit = 2;
        p.thread_list_max_limit = 3;
        p.thread_read_default_turns = 1;
        p.thread_read_max_turns = 2;
        p.max_file_search_results = 2;
    })
    .await;
    let project = env.project("p", false).await;
    for _ in 0..4 {
        env.thread(&project).await;
    }
    let list = |limit: Option<u32>| ThreadListParams {
        project_id: None,
        include_archived: false,
        limit,
        before: None,
    };
    let page = env.call::<spec::ThreadList>(list(None)).await.unwrap();
    assert_eq!((page.threads.len(), page.has_more), (2, true));
    assert_eq!(
        env.call::<spec::ThreadList>(list(Some(100)))
            .await
            .unwrap()
            .threads
            .len(),
        3,
        "capped at the maximum"
    );

    let thread = env.thread(&project).await;
    for (i, text) in ["one", "two", "three"].iter().enumerate() {
        env.send(&thread.id, text, Delivery::Auto).await;
        env.wait_for(
            &thread_stream(&thread.id),
            0,
            |e| matches!(e, Event::TurnCompleted { turn } if turn.index == i as u32),
        )
        .await;
    }
    let read = |limit: Option<u32>| ThreadReadParams {
        thread_id: thread.id.clone(),
        before_turn_index: None,
        limit_turns: limit,
    };
    let latest = env.call::<spec::ThreadRead>(read(None)).await.unwrap();
    assert_eq!((latest.turns.len(), latest.has_more_before), (1, true));
    assert_eq!(
        env.call::<spec::ThreadRead>(read(Some(50)))
            .await
            .unwrap()
            .turns
            .len(),
        2,
        "capped at the maximum"
    );

    for name in ["a.rs", "ab.rs", "abc.rs"] {
        std::fs::write(Path::new(&project.path).join(name), b"").unwrap();
    }
    let found = env
        .call::<spec::FsSearch>(FsSearchParams {
            project_id: Some(project.id.clone()),
            thread_id: None,
            query: "a".into(),
            limit: Some(50),
        })
        .await
        .unwrap();
    assert_eq!(found.results.len(), 2, "capped at max_file_search_results");
}
