//! Live tests against a real ACP agent (Devin CLI by default). They spend tokens, so they
//! run only with `AAS_LIVE_TESTS=1 cargo test -p aas-adapter-acp -- --ignored`.
//!
//! Environment:
//! * `AAS_ACP_COMMAND` / `AAS_ACP_ARGS` (space separated): agent command, default `devin acp`.
//! * `AAS_ACP_MODEL`: model id to select, default `swe-1-7-lightning-medium` (Devin's default
//!   model may be rate limited on free plans). The background test uses the agent's own
//!   default model unless this is set.
//!
//! The background test needs an agent that confirms Cognition's background extension (Devin
//! CLI), the fork test its revert extension; each deletes the sessions it created
//! (`session/delete`) when it ends.

mod common;

use std::path::Path;
use std::time::Duration;

use aas_adapter_acp::AcpAdapter;
use aas_harness::protocol::{
    ApprovalOptionKind, BackgroundTaskKind, HarnessKind, InteractionRequest, InteractionResolution,
    ItemBody, ItemStatus, Subject, ThreadId, TurnStatus,
};
use aas_harness::{
    AdapterContext, AdapterError, AdapterEvent, AdapterPolicy, BackgroundState, CommandContext,
    ForkPoint, HarnessAdapter, HarnessConfig, SessionHandle, StartMode, StartOptions, StartRequest,
    StopReason, ThreadSettings, TurnInput,
};
use aas_stdio::{Incoming, RpcPeer, RpcPeerConfig, RpcWireError};
use aas_supervisor::{SpawnSpec, Supervisor, SupervisorPolicy, resolve_program};
use common::*;
use serde_json::{Value, json};

fn live() -> bool {
    let on = std::env::var_os("AAS_LIVE_TESTS").is_some();
    if !on {
        eprintln!("skipped: set AAS_LIVE_TESTS=1 to run live tests");
    }
    on
}

struct Env {
    adapter: AcpAdapter,
    supervisor: Supervisor,
    work: tempfile::TempDir,
    state: tempfile::TempDir,
    model: Option<String>,
    command: String,
    args: Vec<String>,
}

impl Env {
    /// Another adapter for the same agent on the same supervisor (another daemon, as far as
    /// what its sessions listed goes), with its own state folder `name`.
    fn other_adapter(&self, name: &str) -> AcpAdapter {
        adapter(
            &self.supervisor,
            &self.state.path().join(name),
            &self.command,
            &self.args,
        )
    }
}

fn adapter(
    supervisor: &Supervisor,
    state_dir: &Path,
    command: &str,
    args: &[String],
) -> AcpAdapter {
    let ctx = AdapterContext {
        supervisor: supervisor.clone(),
        state_dir: state_dir.to_path_buf(),
        policy: AdapterPolicy {
            stop_grace: Duration::from_secs(5),
            ..AdapterPolicy::default()
        },
    };
    let config = HarnessConfig {
        id: "devin".into(),
        kind: HarnessKind::Acp,
        display_name: Some("Devin".into()),
        command: command.to_owned(),
        args: args.to_vec(),
        env: Default::default(),
        options: serde_json::json!({ "auth_hint": "Run `devin auth login`." }),
    };
    AcpAdapter::new(config, ctx)
}

fn env() -> Env {
    let state = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let supervisor = Supervisor::new(
        state.path(),
        SupervisorPolicy {
            prevent_sleep: false,
            ..SupervisorPolicy::default()
        },
    )
    .unwrap();
    let command = std::env::var("AAS_ACP_COMMAND").unwrap_or_else(|_| "devin".into());
    let args: Vec<String> = std::env::var("AAS_ACP_ARGS")
        .unwrap_or_else(|_| "acp".into())
        .split_whitespace()
        .map(str::to_owned)
        .collect();
    let model = match std::env::var("AAS_ACP_MODEL") {
        Ok(m) if m.is_empty() => None,
        Ok(m) => Some(m),
        Err(_) => Some("swe-1-7-lightning-medium".into()),
    };
    Env {
        adapter: adapter(&supervisor, &state.path().join("adapter"), &command, &args),
        supervisor,
        work,
        state,
        model,
        command,
        args,
    }
}

/// Deletes a session the test created (`session/delete`, when the agent advertises
/// `sessionCapabilities.delete`) through a short-lived supervised agent process.
async fn delete_session(env: &Env, cwd: &Path, session_id: &str) {
    let program = resolve_program(&env.command).expect("agent command");
    let spec = SpawnSpec::new("acp[live-cleanup]", program, cwd).args(env.args.iter());
    let mut child = env.supervisor.spawn(spec).await.expect("spawn");
    let (stdin, stdout) = (child.stdin.take().unwrap(), child.stdout.take().unwrap());
    let (peer, mut incoming) = RpcPeer::start(
        stdout,
        stdin,
        RpcPeerConfig {
            emit_jsonrpc_field: true,
            max_line_bytes: AdapterPolicy::default().max_line_bytes,
            label: "acp[live-cleanup]".into(),
        },
    );
    {
        let peer = peer.clone();
        tokio::spawn(async move {
            while let Some(msg) = incoming.recv().await {
                if let Incoming::Request(req) = msg {
                    peer.respond_error(req.id, RpcWireError::method_not_found(&req.method))
                        .await
                        .expect("refuse an agent request");
                }
            }
        });
    }
    let timeout = Duration::from_secs(60);
    let init: Value = peer
        .request_timeout(
            "initialize",
            json!({"protocolVersion": 1,
                   "clientCapabilities": {"fs": {"readTextFile": false, "writeTextFile": false}, "terminal": false},
                   "clientInfo": {"name": "aas-live-cleanup", "version": "0"}}),
            timeout,
        )
        .await
        .expect("initialize");
    if init["agentCapabilities"]["sessionCapabilities"]["delete"].is_object() {
        peer.request_timeout::<_, Value>(
            "session/delete",
            json!({ "sessionId": session_id }),
            timeout,
        )
        .await
        .expect("session/delete");
    } else {
        eprintln!("the agent cannot delete sessions; {session_id} is left in its store");
    }
    peer.close_writer().await;
    child
        .handle
        .shutdown(Duration::from_secs(5), StopReason::Shutdown)
        .await;
}

async fn run_turn(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<AdapterEvent>,
    f: &mut Folded,
    mut on: impl FnMut(&AdapterEvent),
) {
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(240), rx.recv())
            .await
            .expect("turn timed out")
            .expect("events closed");
        on(&ev);
        f.apply(ev.clone());
        if is_turn_completed(&ev) {
            return;
        }
    }
}

#[tokio::test]
#[ignore]
async fn live_turn_list_history_and_cache() {
    if !live() {
        return;
    }
    let env = env();
    let info = env.adapter.probe().await;
    assert!(info.available, "unavailable: {:?}", info.unavailable_reason);
    assert!(info.capabilities.approvals && info.capabilities.interrupt);

    let handle = env
        .adapter
        .start(StartRequest {
            thread_id: ThreadId::generate(),
            cwd: env.work.path().to_path_buf(),
            settings: ThreadSettings {
                model: env.model.clone(),
                ..ThreadSettings::default()
            },
            mode: StartMode::New,
        })
        .await
        .expect("start");
    let session_id = handle.native_session_id.clone().expect("session id");
    let mut rx = handle.events;
    let mut f = Folded::default();
    handle
        .control
        .send(TurnInput::text("Reply with exactly: OK"))
        .await
        .unwrap();
    run_turn(&mut rx, &mut f, |_| {}).await;
    let (status, usage, error) = &f.turns[0];
    assert_eq!(*status, TurnStatus::Completed, "{error:?}");
    assert!(usage.is_some());
    // Devin reports the context occupancy with `usage_update`.
    let context = usage.and_then(|u| u.context).expect("context reported");
    assert!(
        context.used_tokens > 0 && context.window_tokens >= context.used_tokens,
        "{context:?}"
    );
    assert!(
        f.items
            .iter()
            .any(|(_, b, _)| matches!(b, ItemBody::AgentMessage { text } if text.contains("OK")))
    );
    handle.control.shutdown(StopReason::User).await;
    drain_to_exit(&mut rx, &mut f).await;

    // The session taught the cache the agent's options and commands.
    let info = env.adapter.probe().await;
    assert!(
        !info.models.is_empty(),
        "models are learned from the first session"
    );
    assert!(!info.permission_modes.is_empty());
    let commands = env
        .adapter
        .commands(CommandContext {
            cwd: env.work.path().to_path_buf(),
            native_session_id: None,
            project_trusted: None,
        })
        .await
        .unwrap();
    assert!(!commands.is_empty());

    let sessions = env
        .adapter
        .list_native_sessions(env.work.path())
        .await
        .unwrap();
    assert!(
        sessions.iter().any(|s| s.native_session_id == session_id),
        "{sessions:?}"
    );
    let history = env
        .adapter
        .read_native_history(env.work.path(), &session_id)
        .await
        .unwrap();
    assert_eq!(history.turns.len(), 1);
    assert!(
        matches!(&history.turns[0].items[0].body, ItemBody::UserMessage { text, .. } if text == "Reply with exactly: OK")
    );

    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        env.supervisor.running_count(),
        0,
        "no agent process may remain"
    );
    delete_session(&env, env.work.path(), &session_id).await;
}

#[tokio::test]
#[ignore]
async fn live_permission_request_can_be_rejected() {
    if !live() {
        return;
    }
    let env = env();
    let handle = env
        .adapter
        .start(StartRequest {
            thread_id: ThreadId::generate(),
            cwd: env.work.path().to_path_buf(),
            settings: ThreadSettings {
                model: env.model.clone(),
                ..ThreadSettings::default()
            },
            mode: StartMode::New,
        })
        .await
        .expect("start");
    let session_id = handle.native_session_id.clone().expect("session id");
    let control = handle.control.clone();
    let mut rx = handle.events;
    let mut f = Folded::default();
    control
        .send(TurnInput::text(
            "Run the shell command `ping -n 2 127.0.0.1` exactly once. If you are not allowed to run it, reply with the single word DENIED and do nothing else.",
        ))
        .await
        .unwrap();
    // The turn blocks on the request, so answer it (dismiss = the agent's reject option).
    let mut requests = Vec::new();
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(240), rx.recv())
            .await
            .expect("turn timed out")
            .expect("events closed");
        f.apply(ev.clone());
        match &ev {
            AdapterEvent::InteractionRequested {
                request_id,
                request,
                ..
            } => {
                requests.push(request.clone());
                control
                    .respond(request_id, &InteractionResolution::Dismissed)
                    .await
                    .unwrap();
            }
            AdapterEvent::TurnCompleted { .. } => break,
            _ => {}
        }
    }
    assert!(
        !requests.is_empty(),
        "the agent ran ping without asking (check the agent's permission mode)"
    );
    for request in &requests {
        assert!(
            matches!(
                request,
                InteractionRequest::Approval {
                    subject: Subject::Command { .. },
                    ..
                }
            ),
            "{request:?}"
        );
    }
    assert_eq!(f.turns[0].0, TurnStatus::Completed, "{:?}", f.turns[0].2);
    control.shutdown(StopReason::User).await;
    drain_to_exit(&mut rx, &mut f).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(env.supervisor.running_count(), 0);
    delete_session(&env, env.work.path(), &session_id).await;
}

/// The prompt of the recorded runs (docs/adapters/acp.md §16): a background shell and a
/// background sub-agent, each running a long `ping`.
const BACKGROUND_PROMPT: &str = "This is a protocol test of background work. Do exactly the following two things, then end your turn right away:\n1. Start a background shell: use your shell/exec tool so that it returns immediately and keeps running in the background (timeout 0 / run in background), running exactly: ping -n 300 127.0.0.1\n2. Start ONE background subagent (is_background=true) titled \"long-pinger\" with this task: \"Run the shell command `ping -n 300 127.0.0.1` once in the foreground and wait for it to finish, then reply with the single word PONG-DONE.\"\nDo not wait for either of them, do not read their output, and do not start anything else. After starting both, reply with the single word STARTED and end your turn.";

async fn next_event(rx: &mut tokio::sync::mpsc::UnboundedReceiver<AdapterEvent>) -> AdapterEvent {
    tokio::time::timeout(Duration::from_secs(240), rx.recv())
        .await
        .expect("no event within 240 s")
        .expect("events closed")
}

/// Background work through Cognition's extension, end to end against the real agent: the
/// shell and the sub-agent become tasks (the shell's command item is backgrounded), the
/// sub-agent is stopped while the prompt is open, the shell's output streams after the turn
/// (`terminalPreview`) until it is stopped, and no process is left. The session is deleted
/// afterwards.
#[tokio::test]
#[ignore]
async fn live_background_shell_and_sub_agent_are_tasks_and_stop() {
    if !live() {
        return;
    }
    let env = env();
    let info = env.adapter.probe().await;
    assert!(info.available, "unavailable: {:?}", info.unavailable_reason);
    assert!(
        info.capabilities.background_tasks && info.capabilities.background_stop,
        "the agent did not confirm Cognition's background extension"
    );
    let model = std::env::var("AAS_ACP_MODEL")
        .ok()
        .filter(|m| !m.is_empty());
    let handle = env
        .adapter
        .start(StartRequest {
            thread_id: ThreadId::generate(),
            cwd: env.work.path().to_path_buf(),
            settings: ThreadSettings {
                model,
                ..ThreadSettings::default()
            },
            mode: StartMode::New,
        })
        .await
        .expect("start");
    let session_id = handle.native_session_id.clone().expect("session id");
    let control = handle.control.clone();
    let mut rx = handle.events;
    let mut f = Folded::default();
    control
        .send(TurnInput::text(BACKGROUND_PROMPT))
        .await
        .unwrap();

    let mut stopped_agent: Option<String> = None;
    loop {
        let ev = next_event(&mut rx).await;
        f.apply(ev.clone());
        match &ev {
            AdapterEvent::InteractionRequested {
                request_id,
                request: InteractionRequest::Approval { options, .. },
                ..
            } => {
                // "Allow for this session", so that the sub-agent may run `ping` too (a
                // background sub-agent cannot ask).
                let option = options
                    .iter()
                    .find(|o| o.id == "allow_session")
                    .or_else(|| {
                        options
                            .iter()
                            .find(|o| o.kind == ApprovalOptionKind::AllowOnce)
                    })
                    .expect("an allow option");
                control
                    .respond(
                        request_id,
                        &InteractionResolution::Approval {
                            option_id: option.id.clone(),
                            feedback: None,
                        },
                    )
                    .await
                    .unwrap();
            }
            AdapterEvent::BackgroundTask { .. } if stopped_agent.is_none() => {
                let shell = f
                    .tasks
                    .values()
                    .find(|t| t.kind == BackgroundTaskKind::Shell);
                let agent = f.tasks.values().find(|t| {
                    t.kind == BackgroundTaskKind::Agent && t.state == BackgroundState::Running
                });
                if let (Some(_), Some(agent)) = (shell, agent) {
                    let key = agent.key.clone();
                    control.stop_background(&key).await.unwrap();
                    stopped_agent = Some(key);
                }
            }
            AdapterEvent::TurnCompleted { .. } => break,
            _ => {}
        }
    }
    let (status, _, error) = &f.turns[0];
    assert_eq!(*status, TurnStatus::Completed, "{error:?}");
    let agent_key = stopped_agent.expect("the agent started a background sub-agent and a shell");
    while !f.task(&agent_key).state.is_ended() {
        let ev = next_event(&mut rx).await;
        f.apply(ev);
    }
    // Devin reports a cancelled sub-agent as an unsuccessful run.
    assert_eq!(f.task(&agent_key).state, BackgroundState::Failed);

    let shell = f
        .tasks
        .values()
        .find(|t| t.kind == BackgroundTaskKind::Shell && t.parent_key.is_none())
        .expect("the root agent's shell")
        .clone();
    assert!(shell.stoppable);
    let origin = shell.origin_item_key.clone().expect("launched by an item");
    assert_eq!(f.item(&origin).2, ItemStatus::Backgrounded);
    assert_eq!(
        shell.state,
        BackgroundState::Running,
        "ping -n 300 outlives the turn"
    );
    // While it runs, Devin's `terminalPreview` snapshots stream its output into the task.
    while !f
        .outputs
        .get(&shell.key)
        .is_some_and(|o| o.contains("127.0.0.1"))
    {
        let ev = next_event(&mut rx).await;
        f.apply(ev);
    }
    control.stop_background(&shell.key).await.unwrap();
    while !f.task(&shell.key).state.is_ended() {
        let ev = next_event(&mut rx).await;
        f.apply(ev);
    }
    let ended = f.task(&shell.key).clone();
    assert!(
        ended.result.as_ref().and_then(|r| r.exit_code).is_some(),
        "{ended:?}"
    );
    // What the agent reported, for the record of a live run.
    for t in &f.task_events {
        eprintln!(
            "task {} {:?} {:?} live={} runs={} title={:?} origin={:?} parent={:?} progress={:?} exit={:?} summary={:?}",
            t.key,
            t.kind,
            t.state,
            t.live,
            t.runs,
            t.title,
            t.origin_item_key,
            t.parent_key,
            t.progress,
            t.result.as_ref().and_then(|r| r.exit_code),
            t.result.as_ref().and_then(|r| r.summary.as_deref()),
        );
    }
    // Every task the agent reported has ended by its own signal.
    assert!(
        f.tasks.values().all(|t| t.state.is_ended() && !t.live),
        "{:?}",
        f.tasks
    );

    control.shutdown(StopReason::User).await;
    drain_to_exit(&mut rx, &mut f).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        env.supervisor.running_count(),
        0,
        "no agent process may remain"
    );
    delete_session(&env, env.work.path(), &session_id).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(env.supervisor.running_count(), 0);
}

/// The anchors of a session's turns as the events report them: a `TurnAnchor` per turn, then
/// `TurnAnchorReplaced` naming the anchor it replaces.
#[derive(Default)]
struct Anchors(Vec<Value>);

impl Anchors {
    fn apply(&mut self, ev: &AdapterEvent) {
        match ev {
            AdapterEvent::TurnAnchor { anchor } => self.0.push(anchor.clone()),
            AdapterEvent::TurnAnchorReplaced { previous, anchor } => {
                let turn = self
                    .0
                    .iter_mut()
                    .find(|a| *a == previous)
                    .unwrap_or_else(|| panic!("no turn anchored at {previous}"));
                *turn = anchor.clone();
            }
            _ => {}
        }
    }

    fn settled(&self, turn: usize) -> bool {
        self.0
            .get(turn)
            .is_some_and(|a| a.get("forkTargetNodeId").is_some())
    }
}

/// Runs one turn to its end (answering nothing: the prompts need no approval) and, when the
/// turn made a step, until its anchor holds the node ids.
async fn live_turn(
    handle: &mut SessionHandle,
    f: &mut Folded,
    anchors: &mut Anchors,
    prompt: &str,
) {
    handle.control.send(TurnInput::text(prompt)).await.unwrap();
    let before = anchors.0.len();
    loop {
        let ev = next_event(&mut handle.events).await;
        anchors.apply(&ev);
        f.apply(ev.clone());
        if is_turn_completed(&ev) {
            break;
        }
    }
    let (status, _, error) = f.turns.last().unwrap();
    assert_eq!(*status, TurnStatus::Completed, "{prompt}: {error:?}");
    if anchors.0.len() > before {
        while !anchors.settled(before) {
            let ev = next_event(&mut handle.events).await;
            anchors.apply(&ev);
            f.apply(ev);
        }
    }
}

/// A fork of `source` at `point` (`None`: the whole session) by `adapter`.
async fn live_fork(
    adapter: &AcpAdapter,
    cwd: &Path,
    source: &str,
    point: Option<ForkPoint>,
) -> Result<SessionHandle, AdapterError> {
    adapter
        .start_with(
            StartRequest {
                thread_id: ThreadId::generate(),
                cwd: cwd.to_path_buf(),
                settings: ThreadSettings::default(),
                mode: StartMode::Fork {
                    native_session_id: source.to_owned(),
                },
            },
            StartOptions {
                fork_at: point,
                ..StartOptions::default()
            },
        )
        .await
}

/// Stops a session and returns what its history says: the user messages and the anchors.
async fn live_history(
    adapter: &AcpAdapter,
    cwd: &Path,
    mut handle: SessionHandle,
) -> (String, Vec<String>, Vec<Option<Value>>) {
    let id = handle.native_session_id.clone().expect("session id");
    handle.control.shutdown(StopReason::User).await;
    let mut f = Folded::default();
    drain_to_exit(&mut handle.events, &mut f).await;
    let (history, anchors) = adapter
        .read_native_history_anchored(cwd, &id)
        .await
        .expect("history");
    let users = history
        .turns
        .iter()
        .filter_map(|t| match &t.items.first()?.body {
            ItemBody::UserMessage { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect();
    (id, users, anchors)
}

/// Cognition's other extensions end to end against the real agent (docs/adapters/acp.md §17):
/// the features, a rename, Devin's statistics, its own mode commands, and forks at a turn
/// (with the turn, before it, of the whole session) while the source's process holds it and
/// after it stopped, by the adapter that ran the source (it listed the nodes) and by another
/// one (which has to read the source, and cannot while it is held). Every session is deleted
/// afterwards.
#[tokio::test]
#[ignore]
async fn live_forks_at_turns_rename_status_and_modes() {
    if !live() {
        return;
    }
    let env = env();
    let cwd = env.work.path().to_path_buf();
    let info = env.adapter.probe().await;
    assert!(info.available, "unavailable: {:?}", info.unavailable_reason);
    let features = env.adapter.features();
    assert!(
        features.fork_at_turn && features.fork_while_held && features.rename && features.status,
        "{features:?}"
    );
    assert!(features.plan_mode.is_none() && !features.move_to_background);
    assert!(info.capabilities.fork);

    let mut handle = env
        .adapter
        .start(StartRequest {
            thread_id: ThreadId::generate(),
            cwd: cwd.clone(),
            settings: ThreadSettings {
                model: env.model.clone(),
                ..ThreadSettings::default()
            },
            mode: StartMode::New,
        })
        .await
        .expect("start");
    let source = handle.native_session_id.clone().expect("session id");
    let mut created = vec![source.clone()];
    let mut f = Folded::default();
    let mut anchors = Anchors::default();

    handle.control.rename("aas live fork source").await.unwrap();
    live_turn(&mut handle, &mut f, &mut anchors, "Reply with exactly: ONE").await;
    live_turn(&mut handle, &mut f, &mut anchors, "Reply with exactly: TWO").await;
    assert_eq!(anchors.0.len(), 2, "{:?}", anchors.0);
    assert!(anchors.settled(0) && anchors.settled(1), "{:?}", anchors.0);
    assert!(f.events.iter().any(
        |e| matches!(e, AdapterEvent::SessionTitle { title } if title == "aas live fork source")
    ));
    // Devin's statistics of the last turn.
    let status = handle.control.status().await.unwrap();
    eprintln!("status: {status:?}");
    assert!(
        status
            .iter()
            .any(|s| s.title.ends_with("(last turn)") && !s.rows.is_empty()),
        "{status:?}"
    );
    // Devin's own mode commands (no model call, no step): reported as the permission mode.
    live_turn(&mut handle, &mut f, &mut anchors, "/ask").await;
    assert_eq!(f.infos.last().and_then(|i| i.1.as_deref()), Some("ask"));
    live_turn(&mut handle, &mut f, &mut anchors, "/code").await;
    assert_eq!(
        f.infos.last().and_then(|i| i.1.as_deref()),
        Some("accept-edits")
    );
    assert_eq!(anchors.0.len(), 2, "mode commands make no step");
    // Devin's account commands are not offered.
    assert!(
        f.commands
            .iter()
            .all(|c| !c.iter().any(|n| n == "login" || n == "logout")),
        "{:?}",
        f.commands
    );
    let commands = env
        .adapter
        .commands(CommandContext {
            cwd: cwd.clone(),
            native_session_id: None,
            project_trusted: None,
        })
        .await
        .unwrap();
    assert!(
        commands
            .iter()
            .all(|c| c.name != "login" && c.name != "logout")
            && commands.iter().any(|c| c.name == "plan")
    );

    let (one, two) = (anchors.0[0].clone(), anchors.0[1].clone());
    let at = |anchor: &Value, before: bool, previous: Option<&Value>| ForkPoint {
        anchor: anchor.clone(),
        before,
        previous: previous.cloned(),
    };
    // While the source's process holds it: with the first turn, before the second, the whole
    // session.
    let mut forks = Vec::new();
    for (point, expected) in [
        (Some(at(&one, false, None)), vec!["Reply with exactly: ONE"]),
        (
            Some(at(&two, true, Some(&one))),
            vec!["Reply with exactly: ONE"],
        ),
        (
            None,
            vec!["Reply with exactly: ONE", "Reply with exactly: TWO"],
        ),
    ] {
        let fork = live_fork(&env.adapter, &cwd, &source, point)
            .await
            .expect("fork");
        let (id, users, fork_anchors) = live_history(&env.adapter, &cwd, fork).await;
        created.push(id.clone());
        assert_eq!(users, expected, "{id}");
        // The branch keeps the source's steps and nodes.
        assert_eq!(fork_anchors[0].as_ref(), Some(&one), "{id}");
        forks.push(id);
    }
    // Another adapter did not list the source's nodes: a provisional anchor needs the source
    // read, which Devin refuses while the source is held.
    let other = env.other_adapter("other");
    assert!(other.probe().await.available);
    let provisional = json!({ "stepIds": one["stepIds"].clone() });
    let err = live_fork(&other, &cwd, &source, Some(at(&provisional, false, None)))
        .await
        .err()
        .expect("a held source cannot be read");
    eprintln!("held: {err}");
    assert!(matches!(err, AdapterError::Harness(_)), "{err:?}");

    // The source stops: its history carries the same anchors, and the other adapter can read
    // it now.
    let (_, users, source_anchors) = live_history(&env.adapter, &cwd, handle).await;
    assert_eq!(users.len(), 2, "{users:?}");
    assert_eq!(source_anchors, vec![Some(one.clone()), Some(two.clone())]);
    let fork = live_fork(&other, &cwd, &source, Some(at(&provisional, false, None)))
        .await
        .expect("fork after reading the source");
    let (id, users, _) = live_history(&other, &cwd, fork).await;
    created.push(id);
    assert_eq!(users, vec!["Reply with exactly: ONE"]);

    for id in &created {
        eprintln!("created session {id}");
        delete_session(&env, &cwd, id).await;
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        env.supervisor.running_count(),
        0,
        "no agent process may remain"
    );
}
