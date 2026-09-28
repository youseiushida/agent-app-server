//! Live tests against the real `claude` CLI. They spend tokens, so they only run with
//! `AAS_LIVE_TESTS=1 cargo test -p aas-adapter-claude -- --ignored`.

use std::time::Duration;

use aas_adapter_claude::ClaudeAdapter;
use aas_harness::protocol::{
    HarnessKind, InteractionRequest, InteractionResolution, ItemBody, ItemStatus, ThreadSettings,
    TurnStatus,
};
use aas_harness::{
    AdapterContext, AdapterEvent, AdapterPolicy, CommandContext, HarnessAdapter, HarnessConfig,
    StartMode, StartRequest, StopReason, ThreadId, TurnInput,
};
use aas_supervisor::{Supervisor, SupervisorPolicy};
use tokio::sync::mpsc::UnboundedReceiver;

/// Removes what a live test's sessions left: their transcripts (and their project folder when
/// empty), their `session-env` folders and the output files of their background tasks
/// (`%TEMP%\claude\<project>\<session id>`), so repeated runs do not clutter the user's
/// Claude Code history. Only folders and files named after the given session ids are touched.
fn remove_transcripts(session_ids: &[String]) {
    let config = std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(std::path::PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".claude")));
    if let Some(config) = &config {
        for id in session_ids {
            let _ = std::fs::remove_dir_all(config.join("session-env").join(id));
        }
    }
    if let Ok(projects) = std::fs::read_dir(std::env::temp_dir().join("claude")) {
        for project in projects.flatten() {
            let mut removed = false;
            for id in session_ids {
                removed |= std::fs::remove_dir_all(project.path().join(id)).is_ok();
            }
            if removed && std::fs::read_dir(project.path()).is_ok_and(|mut d| d.next().is_none()) {
                let _ = std::fs::remove_dir(project.path());
            }
        }
    }
    let root = config.map(|d| d.join("projects"));
    let Some(root) = root else { return };
    let Ok(dirs) = std::fs::read_dir(&root) else {
        return;
    };
    for dir in dirs.flatten() {
        let mut removed = false;
        for id in session_ids {
            let file = dir.path().join(format!("{id}.jsonl"));
            if file.is_file() {
                removed |= std::fs::remove_file(&file).is_ok();
                let _ = std::fs::remove_dir_all(dir.path().join(id));
            }
        }
        // Only a folder this test emptied is removed.
        if removed && std::fs::read_dir(dir.path()).is_ok_and(|mut d| d.next().is_none()) {
            let _ = std::fs::remove_dir(dir.path());
        }
    }
}

fn live() -> bool {
    std::env::var("AAS_LIVE_TESTS").is_ok_and(|v| v == "1")
}

fn adapter(state: &std::path::Path) -> (ClaudeAdapter, Supervisor) {
    let supervisor = Supervisor::new(
        &state.join("supervisor"),
        SupervisorPolicy {
            prevent_sleep: false,
            ..Default::default()
        },
    )
    .unwrap();
    let ctx = AdapterContext {
        supervisor: supervisor.clone(),
        state_dir: state.join("claude"),
        policy: AdapterPolicy::default(),
    };
    let config = HarnessConfig {
        id: "claude".into(),
        kind: HarnessKind::Claude,
        display_name: None,
        command: "claude".into(),
        args: Vec::new(),
        env: Default::default(),
        options: serde_json::Value::Null,
    };
    (ClaudeAdapter::new(config, ctx), supervisor)
}

async fn until_turn_end(
    events: &mut UnboundedReceiver<AdapterEvent>,
    mut on: impl FnMut(&AdapterEvent),
) -> Vec<AdapterEvent> {
    let mut out = Vec::new();
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(180), events.recv())
            .await
            .expect("turn timed out")
            .expect("stream closed");
        on(&ev);
        let end = matches!(ev, AdapterEvent::TurnCompleted { .. });
        out.push(ev);
        if end {
            return out;
        }
    }
}

#[tokio::test]
#[ignore = "spends tokens; set AAS_LIVE_TESTS=1 and pass --ignored"]
async fn live_turn_approval_history_and_clean_exit() {
    if !live() {
        return;
    }
    let state = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let (adapter, supervisor) = adapter(state.path());

    let info = adapter.probe().await;
    assert!(info.available, "{:?}", info.unavailable_reason);
    assert!(
        info.models.iter().any(|m| m.id == "haiku"),
        "{:?}",
        info.models
    );
    assert!(!info.permission_modes.is_empty());

    let settings = ThreadSettings {
        model: Some("haiku".into()),
        effort: None,
        permission_mode: Some("default".into()),
    };
    let mut handle = adapter
        .start(StartRequest {
            thread_id: ThreadId::generate(),
            cwd: work.path().to_path_buf(),
            settings,
            mode: StartMode::New,
        })
        .await
        .unwrap();
    let native = handle.native_session_id.clone().unwrap();

    handle
        .control
        .send(TurnInput::text("Reply with exactly: OK"))
        .await
        .unwrap();
    let events = until_turn_end(&mut handle.events, |_| {}).await;
    assert!(matches!(
        events.last(),
        Some(AdapterEvent::TurnCompleted {
            status: TurnStatus::Completed,
            ..
        })
    ));
    assert!(events.iter().any(|e| matches!(e, AdapterEvent::ItemCompleted { body: Some(ItemBody::AgentMessage { text }), .. } if text.contains("OK"))));
    // The context occupancy Claude Code reports through `get_context_usage`.
    match events.last() {
        Some(AdapterEvent::TurnCompleted {
            usage: Some(usage), ..
        }) => {
            let context = usage.context.expect("context reported");
            assert!(
                context.used_tokens > 0 && context.window_tokens >= context.used_tokens,
                "{context:?}"
            );
        }
        other => panic!("{other:?}"),
    }

    handle
        .control
        .send(TurnInput::text(
            "Use the Bash tool to run `mkdir live-dir`. If it is denied, reply with just: denied",
        ))
        .await
        .unwrap();
    let mut pending = None;
    let events = {
        let control = handle.control.clone();
        let mut out = Vec::new();
        loop {
            let ev = tokio::time::timeout(Duration::from_secs(180), handle.events.recv())
                .await
                .unwrap()
                .unwrap();
            if let AdapterEvent::InteractionRequested {
                request_id,
                request: InteractionRequest::Approval { .. },
                ..
            } = &ev
            {
                pending = Some(request_id.clone());
                control
                    .respond(
                        request_id,
                        &InteractionResolution::Approval {
                            option_id: "deny".into(),
                            feedback: None,
                        },
                    )
                    .await
                    .unwrap();
            }
            let end = matches!(ev, AdapterEvent::TurnCompleted { .. });
            out.push(ev);
            if end {
                break out;
            }
        }
    };
    assert!(
        pending.is_some(),
        "mkdir should ask for approval in default mode"
    );
    assert!(events.iter().any(|e| matches!(
        e,
        AdapterEvent::ItemCompleted {
            status: ItemStatus::Declined,
            ..
        }
    )));
    assert!(!work.path().join("live-dir").exists());

    let commands = adapter
        .commands(CommandContext {
            cwd: work.path().to_path_buf(),
            native_session_id: Some(native.clone()),
        })
        .await
        .unwrap();
    assert!(!commands.is_empty());

    let exit = handle.control.shutdown(StopReason::User).await;
    assert!(
        exit.stopped.is_none(),
        "claude should exit by itself once stdin closes: {exit:?}"
    );
    let mut saw_exit = false;
    while let Ok(Some(ev)) =
        tokio::time::timeout(Duration::from_secs(30), handle.events.recv()).await
    {
        saw_exit |= matches!(ev, AdapterEvent::Exited { .. });
    }
    assert!(saw_exit);
    assert_eq!(
        supervisor.running_count(),
        0,
        "no supervised process may remain"
    );

    let sessions = adapter.list_native_sessions(work.path()).await.unwrap();
    assert!(
        sessions.iter().any(|s| s.native_session_id == native),
        "{sessions:?}"
    );
    let history = adapter
        .read_native_history(work.path(), &native)
        .await
        .unwrap();
    assert_eq!(history.turns.len(), 2);

    // Resume continues the same native session.
    let mut resumed = adapter
        .start(StartRequest {
            thread_id: ThreadId::generate(),
            cwd: work.path().to_path_buf(),
            settings: ThreadSettings {
                model: Some("haiku".into()),
                ..Default::default()
            },
            mode: StartMode::Resume {
                native_session_id: native.clone(),
            },
        })
        .await
        .unwrap();
    resumed
        .control
        .send(TurnInput::text(
            "What was the single word you replied with first? Reply with that word only.",
        ))
        .await
        .unwrap();
    let events = until_turn_end(&mut resumed.events, |_| {}).await;
    assert!(events.iter().any(|e| matches!(e, AdapterEvent::ItemCompleted { body: Some(ItemBody::AgentMessage { text }), .. } if text.contains("OK"))));
    resumed.control.shutdown(StopReason::User).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(supervisor.running_count(), 0);
    remove_transcripts(&[native]);
}

/// Waits for an event matching `pred`, answering every approval with "allow" on the way.
async fn until(
    handle: &mut aas_harness::SessionHandle,
    seen: &mut Vec<AdapterEvent>,
    what: &str,
    pred: impl Fn(&AdapterEvent) -> bool,
) -> AdapterEvent {
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(180), handle.events.recv())
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
            .unwrap_or_else(|| panic!("stream closed waiting for {what}"));
        if let AdapterEvent::InteractionRequested {
            request_id,
            request: InteractionRequest::Approval { .. },
            ..
        } = &ev
        {
            handle
                .control
                .respond(
                    request_id,
                    &InteractionResolution::Approval {
                        option_id: "allow".into(),
                        feedback: None,
                    },
                )
                .await
                .unwrap();
        }
        let hit = pred(&ev);
        seen.push(ev.clone());
        if hit {
            return ev;
        }
    }
}

fn task_of(ev: &AdapterEvent) -> Option<&aas_harness::BackgroundTaskInfo> {
    match ev {
        AdapterEvent::BackgroundTask { task } => Some(task),
        _ => None,
    }
}

/// The background path against the real CLI: a background Bash that `stop_task` stops, a
/// scheduled wakeup (CronCreate) known from its result and the Stop hook's list and cancelled
/// with CronDelete, and ultracode confirmed by `get_settings` (no model call).
#[tokio::test]
#[ignore = "spends tokens; set AAS_LIVE_TESTS=1 and pass --ignored"]
async fn live_background_bash_stop_scheduled_wakeup_and_ultracode() {
    if !live() {
        return;
    }
    let state = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let (adapter, supervisor) = adapter(state.path());
    let info = adapter.probe().await;
    assert!(info.capabilities.background_tasks && info.capabilities.background_stop);
    assert!(
        info.effort_levels.iter().any(|l| l.id == "ultracode"),
        "{:?}",
        info.effort_levels
    );
    let settings = ThreadSettings {
        model: Some("haiku".into()),
        effort: None,
        permission_mode: Some("default".into()),
    };
    let mut handle = adapter
        .start(StartRequest {
            thread_id: ThreadId::generate(),
            cwd: work.path().to_path_buf(),
            settings: settings.clone(),
            mode: StartMode::New,
        })
        .await
        .unwrap();
    let native = handle.native_session_id.clone().unwrap();
    let mut seen = Vec::new();

    // 1. A background Bash: the task (live, with its launching item), the item backgrounded.
    handle
        .control
        .send(TurnInput::text(
            "Use the Bash tool with run_in_background set to true to run exactly this command: sleep 120 . \
             After starting it, end your turn immediately with a one-line reply; do not wait for it or check on it.",
        ))
        .await
        .unwrap();
    until(&mut handle, &mut seen, "the first turn", |e| {
        matches!(e, AdapterEvent::TurnCompleted { .. })
    })
    .await;
    let shell = seen
        .iter()
        .filter_map(task_of)
        .rfind(|t| t.kind == aas_harness::BackgroundTaskKind::Shell)
        .cloned()
        .expect("a background shell was reported");
    assert!(shell.live && shell.stoppable, "{shell:?}");
    let item = shell.origin_item_key.clone().expect("launched by an item");
    assert!(seen.iter().any(|e| matches!(e, AdapterEvent::ItemCompleted { key, status: ItemStatus::Backgrounded, .. } if *key == item)));
    // 2. stop_task: accepted, then the end is reported.
    handle.control.stop_background(&shell.key).await.unwrap();
    let stopped = until(&mut handle, &mut seen, "the stopped shell", |e| {
        task_of(e)
            .is_some_and(|t| t.key == shell.key && t.state == aas_harness::BackgroundState::Stopped)
    })
    .await;
    assert!(!task_of(&stopped).unwrap().live);

    // 3. A scheduled wakeup that never comes due during the test, then its deletion.
    handle
        .control
        .send(TurnInput::text(
            "Call the CronCreate tool exactly once with cron \"0 0 1 1 *\", prompt \"Reply with exactly: never\", \
             recurring false, durable false. If its schema is not loaded, load it with ToolSearch first. \
             Then end your turn with a one-line reply.",
        ))
        .await
        .unwrap();
    until(&mut handle, &mut seen, "the CronCreate turn", |e| {
        matches!(e, AdapterEvent::TurnCompleted { .. })
    })
    .await;
    let wakeup = seen
        .iter()
        .filter_map(task_of)
        .rfind(|t| t.kind == aas_harness::BackgroundTaskKind::Scheduled)
        .cloned()
        .expect("the wakeup was reported");
    assert!(wakeup.live && !wakeup.stoppable, "{wakeup:?}");
    assert_eq!(wakeup.title, "Reply with exactly: never");
    assert!(wakeup.origin_item_key.is_some());
    let id = wakeup.key.trim_start_matches("cron:").to_owned();
    handle
        .control
        .send(TurnInput::text(format!(
            "Call the CronDelete tool exactly once to delete the job with id {id}. If its schema is not loaded, \
             load it with ToolSearch first. Then end your turn with a one-line reply."
        )))
        .await
        .unwrap();
    until(&mut handle, &mut seen, "the deleted wakeup", |e| {
        task_of(e).is_some_and(|t| {
            t.key == wakeup.key && t.state == aas_harness::BackgroundState::Stopped
        })
    })
    .await;
    until(&mut handle, &mut seen, "the CronDelete turn", |e| {
        matches!(e, AdapterEvent::TurnCompleted { .. })
    })
    .await;

    // 4. Ultracode: confirmed on a model with xhigh effort, refused on haiku.
    let sonnet = ThreadSettings {
        model: Some("sonnet".into()),
        effort: Some("ultracode".into()),
        ..settings.clone()
    };
    handle.control.apply_settings(&sonnet).await.unwrap();
    let haiku = ThreadSettings {
        model: Some("haiku".into()),
        ..sonnet.clone()
    };
    match handle.control.apply_settings(&haiku).await {
        Err(aas_harness::AdapterError::Harness(m)) => assert!(m.contains("ultracode"), "{m}"),
        other => panic!("{other:?}"),
    }

    // Every task ended; the triggers are explicit only.
    let exit = handle.control.shutdown(StopReason::User).await;
    let _ = exit;
    while let Ok(Some(ev)) =
        tokio::time::timeout(Duration::from_secs(30), handle.events.recv()).await
    {
        let last = matches!(ev, AdapterEvent::Exited { .. });
        seen.push(ev);
        if last {
            break;
        }
    }
    assert!(matches!(seen.last(), Some(AdapterEvent::Exited { .. })));
    assert!(seen.iter().all(|e| !matches!(
        e,
        AdapterEvent::TurnCompleted {
            trigger: Some(aas_harness::protocol::TurnTrigger::Scheduled),
            ..
        }
    )));
    assert_eq!(
        supervisor.running_count(),
        0,
        "no supervised process may remain"
    );
    remove_transcripts(&[native]);
}

/// A background agent's completion: the CLI starts a run by itself, which the result marks
/// (`origin: task-notification`).
#[tokio::test]
#[ignore = "spends tokens; set AAS_LIVE_TESTS=1 and pass --ignored"]
async fn live_background_agent_completion_starts_a_marked_run() {
    if !live() {
        return;
    }
    let state = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let (adapter, supervisor) = adapter(state.path());
    let mut handle = adapter
        .start(StartRequest {
            thread_id: ThreadId::generate(),
            cwd: work.path().to_path_buf(),
            settings: ThreadSettings {
                model: Some("haiku".into()),
                effort: None,
                permission_mode: Some("default".into()),
            },
            mode: StartMode::New,
        })
        .await
        .unwrap();
    let native = handle.native_session_id.clone().unwrap();
    let mut seen = Vec::new();
    // The agent works longer than the turn that launches it (its foreground command asks for
    // approval, which belongs to the agent), so that its end comes after that turn.
    handle
        .control
        .send(TurnInput::text(
            "Launch exactly ONE background subagent using the Agent tool with run_in_background set to true. \
             The subagent's task: run the Bash command `ping -n 15 127.0.0.1` in the foreground (not in the background), \
             wait for it, then reply with exactly the word done-A. \
             After launching it, end your turn immediately with a one-line reply; do not wait for it.",
        ))
        .await
        .unwrap();
    until(&mut handle, &mut seen, "the first turn", |e| {
        matches!(e, AdapterEvent::TurnCompleted { .. })
    })
    .await;
    let agent = seen
        .iter()
        .filter_map(task_of)
        .find(|t| t.kind == aas_harness::BackgroundTaskKind::Agent)
        .cloned()
        .expect("the agent was reported");
    let ended = |e: &AdapterEvent| {
        task_of(e).is_some_and(|t| {
            t.key == agent.key && t.state == aas_harness::BackgroundState::Completed
        })
    };
    if !seen.iter().any(ended) {
        until(&mut handle, &mut seen, "the agent's end", ended).await;
    }
    // Its permission request belonged to it.
    assert!(
        seen.iter().any(|e| matches!(e, AdapterEvent::InteractionRequested { background_key: Some(k), .. } if *k == agent.key)),
        "the agent asked to run ping"
    );
    let marked = until(&mut handle, &mut seen, "the run the CLI starts", |e| {
        matches!(e, AdapterEvent::TurnCompleted { .. })
    })
    .await;
    assert!(
        matches!(
            marked,
            AdapterEvent::TurnCompleted {
                trigger: Some(aas_harness::protocol::TurnTrigger::BackgroundTask),
                ..
            }
        ),
        "{marked:?}"
    );
    handle.control.shutdown(StopReason::User).await;
    while let Ok(Some(ev)) =
        tokio::time::timeout(Duration::from_secs(30), handle.events.recv()).await
    {
        if matches!(ev, AdapterEvent::Exited { .. }) {
            break;
        }
    }
    assert_eq!(
        supervisor.running_count(),
        0,
        "no supervised process may remain"
    );
    remove_transcripts(&[native]);
}

#[tokio::test]
#[ignore = "spends tokens; set AAS_LIVE_TESTS=1 and pass --ignored"]
async fn live_interrupt_and_shutdown_mid_turn() {
    if !live() {
        return;
    }
    let state = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let (adapter, supervisor) = adapter(state.path());
    let mut handle = adapter
        .start(StartRequest {
            thread_id: ThreadId::generate(),
            cwd: work.path().to_path_buf(),
            settings: ThreadSettings {
                model: Some("haiku".into()),
                ..Default::default()
            },
            mode: StartMode::New,
        })
        .await
        .unwrap();
    let native = handle.native_session_id.clone().unwrap();
    handle
        .control
        .send(TurnInput::text(
            "Write a 300-line poem about rivers. Do not use tools.",
        ))
        .await
        .unwrap();
    // Interrupt as soon as text streams.
    let control = handle.control.clone();
    let mut interrupted = false;
    let events = until_turn_end(&mut handle.events, |ev| {
        if !interrupted && matches!(ev, AdapterEvent::ItemDelta { .. }) {
            interrupted = true;
            let c = control.clone();
            tokio::spawn(async move { c.interrupt().await });
        }
    })
    .await;
    assert!(
        matches!(
            events.last(),
            Some(AdapterEvent::TurnCompleted {
                status: TurnStatus::Interrupted,
                ..
            })
        ),
        "{:?}",
        events.last()
    );

    // Shutdown in the middle of a new turn: the tree is gone afterwards.
    handle
        .control
        .send(TurnInput::text("Count from 1 to 500, one per line."))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    handle.control.shutdown(StopReason::User).await;
    while let Ok(Some(ev)) =
        tokio::time::timeout(Duration::from_secs(30), handle.events.recv()).await
    {
        if matches!(ev, AdapterEvent::Exited { .. }) {
            break;
        }
    }
    assert_eq!(supervisor.running_count(), 0);
    remove_transcripts(&[native]);
}
