//! The extended harness features through the engine (design.md §5.5, §9.5, §9.6), with the
//! in-process fake agent keeping its sessions in a store: typed session-switching commands,
//! a native session switch, settings and modes the harness changes, renames, forks at a turn,
//! the harness status, side questions, moving running work to the background, returned steers,
//! composer text, the project's trust decision, a resume that fails and anchors settled later.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use aas_adapter_fake::FakeAdapter;
use aas_adapter_fake::store::SessionStore;
use aas_core::{Engine, EngineConfig, HarnessRegistry, Policy, RequestCtx};
use aas_harness::{AdapterContext, HarnessConfig, HarnessKind};
use aas_protocol::events::{Event, EventEnvelope};
use aas_protocol::methods::{spec, *};
use aas_protocol::*;
use aas_supervisor::{Supervisor, SupervisorPolicy};

/// Upper bound of every wait for an event (only bounds how long a broken test hangs).
const WAIT: Duration = Duration::from_secs(60);

struct Env {
    _dir: tempfile::TempDir,
    root: PathBuf,
    sessions: PathBuf,
    engine: Arc<Engine>,
    ctx: RequestCtx,
}

async fn env() -> Env {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let root = dir.path().join("projects");
    std::fs::create_dir_all(&root).unwrap();
    let root = dunce::canonicalize(&root).unwrap();
    let sessions = dir.path().join("fake-sessions");
    let policy = Policy {
        stop_grace: Duration::from_millis(500),
        interrupt_grace: Duration::from_millis(1500),
        prevent_sleep_while_running: false,
        ..Policy::default()
    };
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
    let adapter = FakeAdapter::new(
        HarnessConfig {
            id: "fake".into(),
            kind: HarnessKind::Fake,
            display_name: None,
            command: String::new(),
            args: Vec::new(),
            env: Default::default(),
            options: serde_json::json!({
                "mode": "inProcess",
                "sessionsDir": sessions.display().to_string(),
            }),
        },
        ctx,
    );
    let registry = HarnessRegistry::new(vec![Arc::new(adapter)]);
    let config = EngineConfig {
        data_dir: data,
        server_name: "test".into(),
        hostname: "host".into(),
        project_roots: vec![root.clone()],
        policy,
        heuristics: Default::default(),
        git: None,
    };
    let engine = Engine::start(config, registry, supervisor).await.unwrap();
    Env {
        _dir: dir,
        root,
        sessions,
        engine,
        ctx: RequestCtx {
            device_id: DeviceId::from("dev_test"),
        },
    }
}

fn crid() -> String {
    format!("crid-{}", ulid::Ulid::generate())
}

fn text(t: &str) -> Vec<InputPart> {
    vec![InputPart::Text { text: t.into() }]
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

    async fn project(&self) -> Project {
        let dir = self.root.join("p");
        std::fs::create_dir_all(&dir).unwrap();
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

    async fn start(&self, thread: &ThreadId, input: &str) -> Result<TurnStartResult, RpcError> {
        self.call::<spec::TurnStart>(TurnStartParams {
            client_request_id: crid(),
            thread_id: thread.clone(),
            input: text(input),
            delivery: Delivery::Auto,
        })
        .await
    }

    async fn head(&self, thread: &ThreadId) -> u64 {
        self.engine
            .stream_head(&thread_stream(thread))
            .await
            .unwrap()
            .unwrap_or(0)
    }

    /// Reads the thread's stream from `after` until `pred` matches; returns what was read.
    async fn wait_for(
        &self,
        thread: &ThreadId,
        after: u64,
        pred: impl Fn(&Event) -> bool,
    ) -> Vec<EventEnvelope> {
        let stream = thread_stream(thread);
        let mut cursor = after;
        let mut seen = Vec::new();
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            let mut rx = self.engine.subscribe_head(&stream);
            let batch = self
                .engine
                .read_batch(stream.clone(), cursor)
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

    /// Runs a turn to its end: its completed turn and the events of the thread meanwhile.
    async fn turn(&self, thread: &ThreadId, input: &str) -> (Turn, Vec<EventEnvelope>) {
        let after = self.head(thread).await;
        self.start(thread, input).await.unwrap();
        let events = self
            .wait_for(thread, after, |e| matches!(e, Event::TurnCompleted { .. }))
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

    async fn get(&self, thread: &ThreadId) -> Thread {
        self.call::<spec::ThreadGet>(ThreadGetParams {
            thread_id: thread.clone(),
        })
        .await
        .unwrap()
        .thread
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

    async fn update(&self, params: ThreadUpdateParams) -> Result<ThreadUpdateResult, RpcError> {
        self.call::<spec::ThreadUpdate>(params).await
    }

    async fn stop(&self, thread: &ThreadId) {
        self.call::<spec::ThreadStop>(ThreadStopParams {
            client_request_id: crid(),
            thread_id: thread.clone(),
        })
        .await
        .unwrap();
    }

    async fn thread_count(&self) -> usize {
        self.call::<spec::ThreadList>(ThreadListParams::default())
            .await
            .unwrap()
            .threads
            .len()
    }

    async fn fork(
        &self,
        thread: &ThreadId,
        at: Option<&TurnId>,
        before: bool,
    ) -> Result<Thread, RpcError> {
        self.call::<spec::ThreadFork>(ThreadForkParams {
            client_request_id: crid(),
            thread_id: thread.clone(),
            at_turn_id: at.cloned(),
            before,
        })
        .await
        .map(|r| r.thread)
    }
}

fn update(thread: &ThreadId) -> ThreadUpdateParams {
    ThreadUpdateParams {
        client_request_id: crid(),
        thread_id: thread.clone(),
        title: None,
        settings: None,
        pinned: None,
        modes: None,
    }
}

fn user_texts(read: &ThreadReadResult) -> Vec<String> {
    read.items
        .iter()
        .filter_map(|i| match &i.body {
            ItemBody::UserMessage { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

fn agent_texts(events: &[EventEnvelope]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match &e.event {
            Event::ItemCompleted { item } => match &item.body {
                ItemBody::AgentMessage { text } => Some(text.clone()),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

/// Input whose first word is one of the harness's session-switching commands (names and
/// aliases from its own list, and `resume` everywhere) is refused with a typed error on every
/// path it could take to the agent; the commands are not offered either.
#[tokio::test(flavor = "multi_thread")]
async fn typed_session_switching_commands_are_refused() {
    let env = env().await;
    let project = env.project().await;
    let thread = env.thread(&project).await;
    for (input, command) in [
        ("/fake-clear", "fake-clear"),
        ("  /fake-reset and more", "fake-reset"),
        ("/resume", "resume"),
    ] {
        let e = env.start(&thread.id, input).await.unwrap_err();
        assert_eq!(
            e.kind(),
            Some(ErrorKind::SessionSwitchingCommand),
            "{input}"
        );
        let data = e.data.clone().unwrap();
        assert_eq!(data["command"], command);
        assert_eq!(data["harnessId"], "fake");
    }
    // Words after the first, and other commands, go through.
    let (turn, _) = env.turn(&thread.id, "please /fake-clear nothing").await;
    assert_eq!(turn.status, TurnStatus::Completed);
    // While a turn runs: neither queued nor steered.
    env.start(&thread.id, "@sleep 500").await.unwrap();
    for delivery in [Delivery::Queue, Delivery::Steer] {
        let e = env
            .call::<spec::TurnStart>(TurnStartParams {
                client_request_id: crid(),
                thread_id: thread.id.clone(),
                input: text("/fake-reset"),
                delivery,
            })
            .await
            .unwrap_err();
        assert_eq!(e.kind(), Some(ErrorKind::SessionSwitchingCommand));
    }
    // A new thread with such an input is not created.
    let before = env
        .call::<spec::ThreadList>(ThreadListParams::default())
        .await
        .unwrap()
        .threads
        .len();
    let e = env
        .call::<spec::ThreadCreate>(ThreadCreateParams {
            client_request_id: crid(),
            project_id: project.id.clone(),
            harness_id: "fake".into(),
            settings: None,
            workspace: None,
            title: None,
            input: Some(text("/fake-clear")),
        })
        .await
        .unwrap_err();
    assert_eq!(e.kind(), Some(ErrorKind::SessionSwitchingCommand));
    let after = env
        .call::<spec::ThreadList>(ThreadListParams::default())
        .await
        .unwrap()
        .threads
        .len();
    assert_eq!(before, after);
    // Not offered: the command and its alias; the harness's other commands are.
    let names: Vec<String> = env
        .call::<spec::CommandList>(CommandListParams {
            thread_id: Some(thread.id.clone()),
            project_id: None,
            harness_id: None,
        })
        .await
        .unwrap()
        .commands
        .into_iter()
        .map(|c| c.name)
        .collect();
    assert!(names.contains(&"fake-help".to_owned()), "{names:?}");
    assert!(!names.iter().any(|n| n == "fake-clear" || n == "fake-reset"));
}

/// The agent moving to another session by itself is followed and reported: the thread's
/// native session id changes, `thread/nativeSessionChanged` names both, and the running turn
/// gets a notice.
#[tokio::test(flavor = "multi_thread")]
async fn a_native_session_switch_is_followed_and_reported() {
    let env = env().await;
    let project = env.project().await;
    let thread = env.thread(&project).await;
    env.turn(&thread.id, "hello").await;
    let first = env.get(&thread.id).await.native_session_id.unwrap();
    let (_, events) = env.turn(&thread.id, "@switch-session\n@text moved").await;
    let (previous, current) = events
        .iter()
        .find_map(|e| match &e.event {
            Event::NativeSessionChanged {
                previous_native_session_id,
                native_session_id,
            } => Some((
                previous_native_session_id.clone(),
                native_session_id.clone(),
            )),
            _ => None,
        })
        .expect("the switch is reported");
    assert_eq!(previous, first);
    assert_ne!(current, first);
    assert_eq!(env.get(&thread.id).await.native_session_id, Some(current));
    assert!(events.iter().any(|e| matches!(
        &e.event,
        Event::ItemStarted { item } if matches!(&item.body, ItemBody::Notice { code: Some(c), .. } if c == "nativeSessionChanged")
    )));
}

/// Permission mode and effort the harness reports are reflected into the thread's settings
/// (values it does not list are not); a change the user made that waits for the next turn
/// wins over a report that comes meanwhile.
#[tokio::test(flavor = "multi_thread")]
async fn settings_the_harness_changes_are_reflected() {
    let env = env().await;
    let project = env.project().await;
    let thread = env.thread(&project).await;
    assert_eq!(thread.settings.permission_mode.as_deref(), Some("ask"));
    env.turn(&thread.id, "@permission auto\n@effort high").await;
    let settings = env.get(&thread.id).await.settings;
    assert_eq!(settings.permission_mode.as_deref(), Some("auto"));
    assert_eq!(settings.effort.as_deref(), Some("high"));
    env.turn(&thread.id, "@permission weird\n@effort extreme")
        .await;
    let settings = env.get(&thread.id).await.settings;
    assert_eq!(settings.permission_mode.as_deref(), Some("auto"));
    assert_eq!(settings.effort.as_deref(), Some("high"));

    // The user asks for `ask` while a turn runs; the harness reports `auto` later in that turn.
    let after = env.head(&thread.id).await;
    env.start(&thread.id, "@sleep 600\n@permission auto")
        .await
        .unwrap();
    let outcome = env
        .update(ThreadUpdateParams {
            settings: Some(ThreadSettings {
                permission_mode: Some("ask".into()),
                ..ThreadSettings::default()
            }),
            ..update(&thread.id)
        })
        .await
        .unwrap();
    assert_eq!(
        outcome.settings_outcome,
        Some(SettingsOutcome::AppliesNextTurn)
    );
    env.wait_for(&thread.id, after, |e| {
        matches!(e, Event::TurnCompleted { .. })
    })
    .await;
    assert_eq!(
        env.get(&thread.id)
            .await
            .settings
            .permission_mode
            .as_deref(),
        Some("ask")
    );
}

/// Plan mode through the app (`modes.plan`): the agent then answers with a proposed plan, and
/// its own report of leaving plan mode is reflected. Fast mode only with a model that has it;
/// the harness's word on it is shown; a model without it ends it.
#[tokio::test(flavor = "multi_thread")]
async fn plan_and_fast_modes() {
    let env = env().await;
    let project = env.project().await;
    let thread = env.thread(&project).await;
    let harness = env
        .call::<spec::HarnessList>(Empty {})
        .await
        .unwrap()
        .harnesses
        .remove(0);
    let plan_mode = harness
        .features
        .plan_mode
        .clone()
        .expect("plan mode is offered");
    assert_eq!(
        plan_mode.implement_prompt.as_deref(),
        Some("Implement the plan.")
    );
    let set = |plan: Option<bool>, fast: Option<bool>| ThreadUpdateParams {
        modes: Some(ThreadModesUpdate { plan, fast }),
        ..update(&thread.id)
    };
    let r = env.update(set(Some(true), None)).await.unwrap();
    assert!(r.thread.modes.plan);
    let (_, events) = env.turn(&thread.id, "fix the flaky test").await;
    let plan = events
        .iter()
        .find_map(|e| match &e.event {
            Event::ItemCompleted { item } => match &item.body {
                ItemBody::ProposedPlan { text } => Some(text.clone()),
                _ => None,
            },
            _ => None,
        })
        .expect("a proposed plan");
    assert!(plan.contains("fix the flaky test"), "{plan}");
    // The agent leaves plan mode by itself: the thread follows.
    env.turn(&thread.id, "@plan-mode off").await;
    assert!(!env.get(&thread.id).await.modes.plan);

    // Fast mode: on with fake-fast (the running agent applies it and reports `on`).
    let r = env.update(set(None, Some(true))).await.unwrap();
    assert!(r.thread.modes.fast);
    assert_eq!(r.settings_outcome, Some(SettingsOutcome::AppliedLive));
    env.turn(&thread.id, "go").await;
    assert_eq!(
        env.get(&thread.id).await.fast_mode_state.as_deref(),
        Some("on")
    );
    // A model without fast mode ends it, and asking for it then is refused.
    let r = env
        .update(ThreadUpdateParams {
            settings: Some(ThreadSettings {
                model: Some("fake-slow".into()),
                ..ThreadSettings::default()
            }),
            ..update(&thread.id)
        })
        .await
        .unwrap();
    assert!(!r.thread.modes.fast);
    let e = env.update(set(None, Some(true))).await.unwrap_err();
    assert_eq!(e.kind(), Some(ErrorKind::CapabilityUnsupported));
    assert_eq!(e.data.unwrap()["capability"], "fastMode");
}

/// The name the fake agent stored for session `id` (recorded in `cwd`).
fn native_title(env: &Env, cwd: &str, id: &str) -> Option<String> {
    let transcript = SessionStore::new(&env.sessions)
        .read(id)
        .expect("the native session");
    assert_eq!(transcript.cwd, cwd);
    transcript.name
}

/// A user's title reaches the native session: pending while no agent runs, given when the next
/// one starts, right away while one runs. A name the agent gives itself does not replace it.
#[tokio::test(flavor = "multi_thread")]
async fn renames_reach_the_native_session() {
    let env = env().await;
    let project = env.project().await;
    let thread = env.thread(&project).await;
    let r = env
        .update(ThreadUpdateParams {
            title: Some("Before any agent".into()),
            ..update(&thread.id)
        })
        .await
        .unwrap();
    assert_eq!(
        r.native_rename.map(|n| n.status),
        Some(NativeRenameStatus::Pending)
    );
    env.turn(&thread.id, "hello").await;
    let native = env.get(&thread.id).await.native_session_id.unwrap();
    assert_eq!(
        native_title(&env, &thread.cwd, &native).as_deref(),
        Some("Before any agent")
    );
    let r = env
        .update(ThreadUpdateParams {
            title: Some("While it runs".into()),
            ..update(&thread.id)
        })
        .await
        .unwrap();
    assert_eq!(
        r.native_rename.map(|n| n.status),
        Some(NativeRenameStatus::Applied)
    );
    // The agent names its session itself: the user's title stays.
    env.turn(&thread.id, "@rename Agent's own name").await;
    assert_eq!(env.get(&thread.id).await.title, "While it runs");
}

/// Forks at a turn: with the turn, before it (its prompt goes back to the user), and before
/// the first turn (a new session). Each fork's first turn runs on a branch holding exactly the
/// kept turns.
#[tokio::test(flavor = "multi_thread")]
async fn forks_branch_at_any_turn() {
    let env = env().await;
    let project = env.project().await;
    let thread = env.thread(&project).await;
    let mut turns = Vec::new();
    for prompt in ["one", "two", "three"] {
        turns.push(env.turn(&thread.id, prompt).await.0);
    }
    assert!(turns.iter().all(|t| t.forkable), "{turns:?}");
    let read = env.read(&thread.id).await;
    assert!(read.turns.iter().all(|t| t.forkable));
    let native_turns = |id: &str| {
        SessionStore::new(&env.sessions)
            .read(id)
            .unwrap()
            .turns
            .len()
    };
    for (at, before, kept) in [(1usize, false, 2usize), (1, true, 1), (0, true, 0)] {
        let fork = env
            .fork(&thread.id, Some(&turns[at].id), before)
            .await
            .unwrap();
        let history = env.read(&fork.id).await;
        assert_eq!(history.turns.len(), kept, "at {at} before={before}");
        assert_eq!(
            fork.forked_from.as_ref().unwrap().turn_id,
            (kept > 0).then(|| turns[kept - 1].id.clone())
        );
        let (turn, _) = env.turn(&fork.id, "in the fork").await;
        assert_eq!(turn.status, TurnStatus::Completed, "{turn:?}");
        let native = env.get(&fork.id).await.native_session_id.unwrap();
        assert_eq!(native_turns(&native), kept + 1, "at {at} before={before}");
        // A fork of a fork at a copied turn works too (the anchors come along).
        if kept == 2 {
            let copied = env.read(&fork.id).await.turns;
            let again = env
                .fork(&fork.id, Some(&copied[0].id), false)
                .await
                .unwrap();
            env.turn(&again.id, "deeper").await;
            let native = env.get(&again.id).await.native_session_id.unwrap();
            assert_eq!(native_turns(&native), 2);
            env.stop(&again.id).await;
        }
        // Each agent holds one of `policy.max_running_processes` until it is stopped.
        env.stop(&fork.id).await;
    }
    let e = env.fork(&thread.id, None, true).await.unwrap_err();
    assert_eq!(e.kind(), Some(ErrorKind::InvalidParams));
    let e = env
        .fork(&thread.id, Some(&TurnId::from("trn_missing")), false)
        .await
        .unwrap_err();
    assert_eq!(e.kind(), Some(ErrorKind::NotFound));
}

/// The harness status comes from the running agent, else from the harness; side questions
/// need the running agent and are answered while a turn runs, outside the history.
#[tokio::test(flavor = "multi_thread")]
async fn harness_status_and_side_questions() {
    let env = env().await;
    let project = env.project().await;
    let thread = env.thread(&project).await;
    let status = |id: &ThreadId| {
        env.call::<spec::ThreadHarnessStatus>(ThreadHarnessStatusParams {
            thread_id: id.clone(),
        })
    };
    let ask = |id: &ThreadId, q: &str| {
        env.call::<spec::ThreadSideQuestion>(ThreadSideQuestionParams {
            thread_id: id.clone(),
            question: q.into(),
        })
    };
    let idle = status(&thread.id).await.unwrap();
    assert!(!idle.live);
    assert_eq!(idle.sections[0].title, "Fake harness");
    let e = ask(&thread.id, "why?").await.unwrap_err();
    assert_eq!(e.kind(), Some(ErrorKind::InvalidState));

    let after = env.head(&thread.id).await;
    env.start(&thread.id, "@text started\n@sleep 800\n@text done")
        .await
        .unwrap();
    // The agent runs once it has said something.
    env.wait_for(&thread.id, after, |e| {
        matches!(e, Event::ItemCompleted { item } if matches!(&item.body, ItemBody::AgentMessage { text } if text == "started"))
    })
    .await;
    let live = status(&thread.id).await.unwrap();
    assert!(live.live);
    assert_eq!(live.sections[0].title, "Fake agent");
    let answer = ask(&thread.id, "which file?").await.unwrap();
    assert_eq!(answer.answer.as_deref(), Some("side answer: which file?"));
    let e = ask(&thread.id, "   ").await.unwrap_err();
    assert_eq!(e.kind(), Some(ErrorKind::InvalidParams));
    env.wait_for(&thread.id, after, |e| {
        matches!(e, Event::TurnCompleted { .. })
    })
    .await;
    assert!(
        !user_texts(&env.read(&thread.id).await)
            .iter()
            .any(|t| t.contains("which file"))
    );
}

/// A running item the harness reports backgroundable moves to the background on request: it
/// closes as backgrounded and its work goes on as a task. Items that do not run are refused.
#[tokio::test(flavor = "multi_thread")]
async fn running_work_moves_to_the_background() {
    let env = env().await;
    let project = env.project().await;
    let thread = env.thread(&project).await;
    let after = env.head(&thread.id).await;
    env.start(&thread.id, "@tool 5000 npm run dev")
        .await
        .unwrap();
    let events = env
        .wait_for(
            &thread.id,
            after,
            |e| matches!(e, Event::ItemUpdated { item } if item.backgroundable),
        )
        .await;
    let item = events
        .iter()
        .find_map(|e| match &e.event {
            Event::ItemUpdated { item } if item.backgroundable => Some(item.clone()),
            _ => None,
        })
        .unwrap();
    let move_it = |id: &ItemId| {
        env.call::<spec::ItemMoveToBackground>(ItemMoveToBackgroundParams {
            client_request_id: crid(),
            thread_id: thread.id.clone(),
            item_id: id.clone(),
        })
    };
    move_it(&item.id).await.unwrap();
    let events = env
        .wait_for(&thread.id, after, |e| {
            matches!(e, Event::TurnCompleted { .. })
        })
        .await;
    let closed = events
        .iter()
        .find_map(|e| match &e.event {
            Event::ItemCompleted { item: done } if done.id == item.id => Some(done.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(closed.status, ItemStatus::Backgrounded);
    assert!(!closed.backgroundable);
    let task = closed.background_task_id.expect("the task it goes on as");
    assert!(events.iter().any(|e| matches!(
        &e.event,
        Event::BackgroundTaskUpdated { task: t } if t.id == task
    )));
    let e = move_it(&item.id).await.unwrap_err();
    assert_eq!(e.kind(), Some(ErrorKind::InvalidState));
    let e = move_it(&ItemId::from("itm_missing")).await.unwrap_err();
    assert_eq!(e.kind(), Some(ErrorKind::NotFound));
}

/// A steer the harness does not take is shown as not delivered and runs as the next turn;
/// text the harness offers for the composer is relayed.
#[tokio::test(flavor = "multi_thread")]
async fn returned_steers_run_next_and_composer_text_is_relayed() {
    let env = env().await;
    let project = env.project().await;
    let thread = env.thread(&project).await;
    let after = env.head(&thread.id).await;
    // The turn waits for the steer (and returns it): no time window it must arrive in.
    env.start(
        &thread.id,
        "@refuse-steers\n@editor draft for you\n@await-steer",
    )
    .await
    .unwrap();
    env.wait_for(
        &thread.id,
        after,
        |e| matches!(e, Event::ComposerInsert { text } if text == "draft for you"),
    )
    .await;
    let steered = env
        .call::<spec::TurnStart>(TurnStartParams {
            client_request_id: crid(),
            thread_id: thread.id.clone(),
            input: text("take this too"),
            delivery: Delivery::Steer,
        })
        .await
        .unwrap();
    assert_eq!(steered.disposition, Disposition::Steered);
    let events = env
        .wait_for(
            &thread.id,
            after,
            |e| matches!(e, Event::TurnStarted { turn } if turn.index == 1),
        )
        .await;
    assert!(events.iter().any(|e| matches!(
        &e.event,
        Event::ItemUpdated { item } if item.status == ItemStatus::Declined
            && matches!(&item.body, ItemBody::UserMessage { text, .. } if text == "take this too")
    )));
    let next = env
        .wait_for(
            &thread.id,
            after,
            |e| matches!(e, Event::TurnCompleted { turn } if turn.index == 1),
        )
        .await;
    assert!(
        agent_texts(&next)
            .iter()
            .any(|t| t == "echo: take this too"),
        "{:?}",
        agent_texts(&next)
    );
}

/// The names `command/list` returns for a thread, or for a project and the fake harness.
async fn listed(env: &Env, thread: Option<&ThreadId>, project: &Project) -> Vec<String> {
    let params = match thread {
        Some(t) => CommandListParams {
            thread_id: Some(t.clone()),
            project_id: None,
            harness_id: None,
        },
        None => CommandListParams {
            thread_id: None,
            project_id: Some(project.id.clone()),
            harness_id: Some("fake".into()),
        },
    };
    env.call::<spec::CommandList>(params)
        .await
        .unwrap()
        .commands
        .into_iter()
        .map(|c| c.name)
        .collect()
}

/// The user's trust decision for the project reaches the agent when it starts; a change
/// replaces a running agent before its next turn. Command listings without a running agent
/// carry the decision too (the fake lists `fake-project` only for a trusted project).
#[tokio::test(flavor = "multi_thread")]
async fn the_projects_trust_decision_reaches_the_agent() {
    let env = env().await;
    let project = env.project().await;
    let project_command = aas_adapter_fake::agent::PROJECT_COMMAND.to_owned();
    assert!(
        !listed(&env, None, &project)
            .await
            .contains(&project_command)
    );
    let thread = env.thread(&project).await;
    assert!(
        !listed(&env, Some(&thread.id), &project)
            .await
            .contains(&project_command)
    );
    let (_, events) = env.turn(&thread.id, "@trust").await;
    assert_eq!(agent_texts(&events), vec!["project trusted: undecided"]);
    let e = env
        .call::<spec::ProjectUpdate>(ProjectUpdateParams {
            client_request_id: crid(),
            project_id: project.id.clone(),
            name: None,
            defaults: None,
            harness_trust: Some([("nobody".to_owned(), true)].into_iter().collect()),
        })
        .await
        .unwrap_err();
    assert_eq!(e.kind(), Some(ErrorKind::InvalidParams));
    let updated = env
        .call::<spec::ProjectUpdate>(ProjectUpdateParams {
            client_request_id: crid(),
            project_id: project.id.clone(),
            name: None,
            defaults: None,
            harness_trust: Some([("fake".to_owned(), true)].into_iter().collect()),
        })
        .await
        .unwrap()
        .project;
    assert_eq!(updated.harness_trust.get("fake"), Some(&true));
    assert!(
        listed(&env, None, &project)
            .await
            .contains(&project_command)
    );
    let (_, events) = env.turn(&thread.id, "@trust").await;
    assert_eq!(agent_texts(&events), vec!["project trusted: yes"]);
    // From the running agent, and without one.
    assert!(
        listed(&env, Some(&thread.id), &project)
            .await
            .contains(&project_command)
    );
    env.stop(&thread.id).await;
    assert!(
        listed(&env, Some(&thread.id), &project)
            .await
            .contains(&project_command)
    );
}

/// A resume that fails (the session is held by another process) ends the turn as
/// `resumeFailed` with the harness's own words (no prefix of the daemon, no terminal escapes);
/// the thread can still be forked into a new one, which runs.
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_resume_can_be_forked_into_a_new_thread() {
    let env = env().await;
    let project = env.project().await;
    let thread = env.thread(&project).await;
    env.turn(&thread.id, "hello").await;
    env.stop(&thread.id).await;
    let native = env.get(&thread.id).await.native_session_id.unwrap();
    SessionStore::new(&env.sessions).hold(&native).unwrap();
    let (turn, _) = env.turn(&thread.id, "again").await;
    assert_eq!(turn.status, TurnStatus::Failed);
    let error = turn.error.unwrap();
    assert_eq!(error.kind, "resumeFailed");
    assert!(
        error.message.contains("is held by another process"),
        "{error:?}"
    );
    assert!(!error.message.contains('\u{1b}'), "{error:?}");
    assert!(
        !error.message.starts_with("the harness reported an error"),
        "{error:?}"
    );
    let fork = env.fork(&thread.id, None, false).await.unwrap();
    let (turn, _) = env.turn(&fork.id, "carry on").await;
    assert_eq!(turn.status, TurnStatus::Completed);
}

/// A turn whose anchor the harness settles only later (`TurnAnchorReplaced`): the settled
/// anchor replaces the provisional one, of a stored turn as well as of the running one, and
/// forks use it; before it settles, a fork at the turn is refused (the harness would not branch
/// there), without creating a thread.
#[tokio::test(flavor = "multi_thread")]
async fn anchors_settled_later_replace_the_provisional_ones() {
    let env = env().await;
    let project = env.project().await;
    let thread = env.thread(&project).await;
    let native_turns = |id: &str| {
        SessionStore::new(&env.sessions)
            .read(id)
            .unwrap()
            .turns
            .len()
    };
    // Turn 0 settles when turn 1 starts; turn 1 settles while it runs; turn 2 stays
    // provisional until the next turn starts.
    let (first, _) = env.turn(&thread.id, "@late-anchor\n@text one").await;
    assert!(first.forkable, "{first:?}");
    let (second, _) = env
        .turn(&thread.id, "@late-anchor\n@settle-anchor\n@text two")
        .await;
    let (third, _) = env.turn(&thread.id, "@late-anchor\n@text three").await;
    for (at, before, kept) in [(&first, false, 1usize), (&second, true, 1)] {
        let fork = env.fork(&thread.id, Some(&at.id), before).await.unwrap();
        let (turn, _) = env.turn(&fork.id, "in the fork").await;
        assert_eq!(turn.status, TurnStatus::Completed, "{turn:?}");
        let native = env.get(&fork.id).await.native_session_id.unwrap();
        assert_eq!(
            native_turns(&native),
            kept + 1,
            "at {} before={before}",
            at.index
        );
        env.stop(&fork.id).await;
    }

    let before_refusal = env.thread_count().await;
    let early = env
        .fork(&thread.id, Some(&third.id), true)
        .await
        .unwrap_err();
    assert_eq!(early.kind(), Some(ErrorKind::InvalidState), "{early:?}");
    assert!(early.message.contains("has not settled yet"), "{early:?}");
    assert_eq!(env.thread_count().await, before_refusal);

    env.turn(&thread.id, "four").await;
    let settled = env.fork(&thread.id, Some(&third.id), true).await.unwrap();
    let (turn, _) = env.turn(&settled.id, "now").await;
    assert_eq!(turn.status, TurnStatus::Completed, "{turn:?}");
    let native = env.get(&settled.id).await.native_session_id.unwrap();
    assert_eq!(native_turns(&native), 3);
    env.stop(&settled.id).await;
}

/// The user messages of native session `id` of the fake agent's store, oldest first.
fn native_prompts(env: &Env, id: &str) -> Vec<String> {
    SessionStore::new(&env.sessions)
        .read(id)
        .unwrap()
        .turns
        .iter()
        .flat_map(|t| &t.items)
        .filter_map(|i| match &i.body {
            ItemBody::UserMessage { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

/// A turn whose start failed never reached the agent (its session was held: `resumeFailed`,
/// the flow of the app's 再試行). A fork before the turn after it ends right after the turn
/// before the failed one, which the fake agent, like Claude Code, needs as the cut.
#[tokio::test(flavor = "multi_thread")]
async fn a_fork_before_a_turn_skips_turns_that_never_reached_the_agent() {
    let env = env().await;
    let project = env.project().await;
    let thread = env.thread(&project).await;
    env.turn(&thread.id, "one").await;
    env.stop(&thread.id).await;
    let native = env.get(&thread.id).await.native_session_id.unwrap();
    let store = SessionStore::new(&env.sessions);
    store.hold(&native).unwrap();
    let (failed, _) = env.turn(&thread.id, "held").await;
    assert_eq!(failed.error.as_ref().unwrap().kind, "resumeFailed");
    store.release(&native).unwrap();
    let (third, _) = env.turn(&thread.id, "three").await;
    assert_eq!(third.status, TurnStatus::Completed, "{third:?}");
    env.stop(&thread.id).await;

    let fork = env.fork(&thread.id, Some(&third.id), true).await.unwrap();
    assert_eq!(env.read(&fork.id).await.turns.len(), 2);
    let (turn, _) = env.turn(&fork.id, "instead of three").await;
    assert_eq!(turn.status, TurnStatus::Completed, "{turn:?}");
    let branch = env.get(&fork.id).await.native_session_id.unwrap();
    assert_eq!(native_prompts(&env, &branch), ["one", "instead of three"]);
    env.stop(&fork.id).await;
}

/// Anchors recorded before the harness moved the thread to another native session stay with
/// the session they came from: forks at those turns branch it, and the turns stay forkable. A
/// fork before the first turn of the new session is refused: where that session began is not
/// known.
#[tokio::test(flavor = "multi_thread")]
async fn forks_at_turns_before_a_session_switch_branch_the_earlier_session() {
    let env = env().await;
    let project = env.project().await;
    let thread = env.thread(&project).await;
    env.turn(&thread.id, "one").await;
    let (two, _) = env.turn(&thread.id, "two").await;
    let first = env.get(&thread.id).await.native_session_id.unwrap();
    let moved_text = "@switch-session\n@text moved";
    let (moved, _) = env.turn(&thread.id, moved_text).await;
    assert_ne!(env.get(&thread.id).await.native_session_id.unwrap(), first);
    let (four, _) = env.turn(&thread.id, "four").await;
    assert!(env.read(&thread.id).await.turns.iter().all(|t| t.forkable));

    let second = env.get(&thread.id).await.native_session_id.unwrap();
    for (at, before, kept, source) in [
        (&two, false, vec!["one", "two"], &first),
        (&two, true, vec!["one"], &first),
        (&four, true, vec![moved_text], &second),
    ] {
        let fork = env.fork(&thread.id, Some(&at.id), before).await.unwrap();
        let (turn, _) = env.turn(&fork.id, "in the fork").await;
        assert_eq!(turn.status, TurnStatus::Completed, "{turn:?}");
        let branch = env.get(&fork.id).await.native_session_id.unwrap();
        let mut expected: Vec<String> = kept.iter().map(|t| (*t).to_owned()).collect();
        expected.push("in the fork".into());
        assert_eq!(
            native_prompts(&env, &branch),
            expected,
            "at {} before={before}",
            at.index
        );
        let transcript = SessionStore::new(&env.sessions).read(&branch).unwrap();
        assert_eq!(transcript.forked_from.as_ref(), Some(source));
        if !before && at.id == two.id {
            // In the fork, the copied anchors belong to the fork's own session.
            let copied = env.read(&fork.id).await.turns;
            let again = env
                .fork(&fork.id, Some(&copied[0].id), false)
                .await
                .unwrap();
            env.turn(&again.id, "deeper").await;
            let deeper = env.get(&again.id).await.native_session_id.unwrap();
            assert_eq!(native_prompts(&env, &deeper), ["one", "deeper"]);
            let transcript = SessionStore::new(&env.sessions).read(&deeper).unwrap();
            assert_eq!(transcript.forked_from.as_ref(), Some(&branch));
            env.stop(&again.id).await;
        }
        env.stop(&fork.id).await;
    }
    let e = env
        .fork(&thread.id, Some(&moved.id), true)
        .await
        .unwrap_err();
    assert_eq!(e.kind(), Some(ErrorKind::InvalidState), "{e:?}");
}

/// Settings of a form the harness no longer offers (a project default chosen with an earlier
/// version: the permission mode `plan`, now plan mode) are taken in the form it offers now when
/// a thread is created, instead of refusing the thread.
#[tokio::test(flavor = "multi_thread")]
async fn project_defaults_of_an_earlier_form_are_upgraded() {
    let env = env().await;
    let project = env.project().await;
    env.call::<spec::ProjectUpdate>(ProjectUpdateParams {
        client_request_id: crid(),
        project_id: project.id.clone(),
        name: None,
        defaults: Some(ProjectDefaults {
            harness_id: Some("fake".into()),
            permission_mode: Some(aas_adapter_fake::LEGACY_PLAN_MODE.into()),
            ..ProjectDefaults::default()
        }),
        harness_trust: None,
    })
    .await
    .unwrap();
    let thread = env.thread(&project).await;
    assert_eq!(thread.settings.permission_mode.as_deref(), Some("ask"));
    assert!(thread.modes.plan, "{thread:?}");
}
