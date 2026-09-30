//! Live tests against the real `claude` CLI. They spend tokens, so they only run with
//! `AAS_LIVE_TESTS=1 cargo test -p aas-adapter-claude -- --ignored`.

use std::time::Duration;

use aas_adapter_claude::ClaudeAdapter;
use aas_harness::protocol::{
    HarnessKind, InteractionRequest, InteractionResolution, ItemBody, ItemStatus, ThreadSettings,
    TurnStatus,
};
use aas_harness::{
    AdapterContext, AdapterEvent, AdapterPolicy, CommandContext, ForkPoint, HarnessAdapter,
    HarnessConfig, StartMode, StartOptions, StartRequest, StopReason, ThreadId, ThreadModes,
    TurnInput,
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
            project_trusted: None,
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

/// Starts a haiku session in the default permission mode (the tests approve every request).
async fn haiku_session(
    adapter: &ClaudeAdapter,
    work: &std::path::Path,
) -> (aas_harness::SessionHandle, String) {
    let handle = adapter
        .start(StartRequest {
            thread_id: ThreadId::generate(),
            cwd: work.to_path_buf(),
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
    (handle, native)
}

/// Shuts the session down and checks that nothing supervised remains.
async fn shut_down(mut handle: aas_harness::SessionHandle, supervisor: &Supervisor) {
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
}

/// A background Bash that ends by itself: its output is the file the CLI names at its end
/// (`task_notification.output_file`), read by the adapter (nothing is parsed from text).
#[tokio::test]
#[ignore = "spends tokens; set AAS_LIVE_TESTS=1 and pass --ignored"]
async fn live_background_shell_output_is_the_file_its_end_names() {
    if !live() {
        return;
    }
    let state = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let (adapter, supervisor) = adapter(state.path());
    let (mut handle, native) = haiku_session(&adapter, work.path()).await;
    // Removes what the session left even when an assertion fails.
    let cleanup = Cleanup::default();
    cleanup.session(&native);
    let mut seen = Vec::new();
    handle
        .control
        .send(TurnInput::text(
            "Use the Bash tool with run_in_background set to true to run exactly this command: \
             echo LIVE_OUT_START; sleep 15; echo LIVE_OUT_END . \
             After starting it, end your turn immediately with a one-line reply; do not wait for it or check on it.",
        ))
        .await
        .unwrap();
    until(&mut handle, &mut seen, "the shell's turn", |e| {
        matches!(e, AdapterEvent::TurnCompleted { .. })
    })
    .await;
    let shell = seen
        .iter()
        .filter_map(task_of)
        .rfind(|t| t.kind == aas_harness::BackgroundTaskKind::Shell)
        .cloned()
        .expect("a background shell was reported");
    // The end comes with `task_updated` (the state) and `task_notification` (the result, with
    // the output file).
    let ended = |e: &AdapterEvent| {
        task_of(e).is_some_and(|t| t.key == shell.key && t.state.is_ended() && t.result.is_some())
    };
    let end = match seen.iter().rfind(|e| ended(e)).cloned() {
        Some(end) => end,
        None => until(&mut handle, &mut seen, "the shell's end", ended).await,
    };
    let end = task_of(&end).unwrap();
    assert_eq!(
        end.state,
        aas_harness::BackgroundState::Completed,
        "{end:?}"
    );
    let result = end.result.as_ref().expect("the end's result");
    let output = result.output.as_deref().unwrap_or_default();
    assert!(
        output.contains("LIVE_OUT_START") && output.contains("LIVE_OUT_END"),
        "{result:?}"
    );
    assert_eq!(
        result.output_omitted_bytes, None,
        "a small file is read whole"
    );
    // Claude Code streams nothing while the shell runs.
    assert!(
        seen.iter()
            .all(|e| !matches!(e, AdapterEvent::BackgroundOutput { .. })),
        "{seen:#?}"
    );
    shut_down(handle, &supervisor).await;
}

/// A `ScheduleWakeup`: live, with the `scheduledFor` of its result, from that result on (before
/// the turn ends), until it fires: the CLI starts a run by itself, and the Stop hook's list of
/// that run no longer holds it, so it ends as completed.
#[tokio::test]
#[ignore = "spends tokens; set AAS_LIVE_TESTS=1 and pass --ignored"]
async fn live_a_schedule_wakeup_is_live_until_it_fires() {
    if !live() {
        return;
    }
    let state = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let (adapter, supervisor) = adapter(state.path());
    let (mut handle, native) = haiku_session(&adapter, work.path()).await;
    // Removes what the session left even when an assertion fails.
    let cleanup = Cleanup::default();
    cleanup.session(&native);
    let mut seen = Vec::new();
    let prompt = "Reply with exactly: woke-up";
    handle
        .control
        .send(TurnInput::text(format!(
            "This is a test of the ScheduleWakeup tool. Call the ScheduleWakeup tool exactly once with delaySeconds 60, \
             reason \"live test\", prompt \"{prompt}\", and noop false. If the tool's schema is not loaded yet, load it \
             with ToolSearch first. After the ScheduleWakeup call returns, end your turn immediately with a one-line reply."
        )))
        .await
        .unwrap();
    until(&mut handle, &mut seen, "the scheduling turn", |e| {
        matches!(e, AdapterEvent::TurnCompleted { .. })
    })
    .await;
    let turn_end = seen.len() - 1;
    let (at, wakeup) = seen
        .iter()
        .enumerate()
        .find_map(|(i, e)| {
            task_of(e)
                .filter(|t| t.kind == aas_harness::BackgroundTaskKind::Scheduled)
                .map(|t| (i, t.clone()))
        })
        .expect("the wakeup was reported");
    assert!(
        at < turn_end,
        "live from the tool result, before the turn ended"
    );
    assert!(wakeup.key.starts_with("wakeup:"), "{wakeup:?}");
    assert!(
        wakeup.live && !wakeup.stoppable && wakeup.state == aas_harness::BackgroundState::Running,
        "{wakeup:?}"
    );
    assert_eq!(wakeup.title, prompt);
    assert!(wakeup.origin_item_key.is_some());
    let due = wakeup.next_run_at.expect("scheduledFor");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis();
    assert!(
        u128::try_from(due).is_ok_and(|due| due > now),
        "comes due later: {due} (now {now})"
    );
    // Still live after the turn (the Stop hook's list holds it).
    let latest = |seen: &[AdapterEvent]| {
        seen.iter()
            .filter_map(task_of)
            .rfind(|t| t.key == wakeup.key)
            .cloned()
            .unwrap()
    };
    assert!(latest(&seen).live, "{:?}", latest(&seen));

    // It fires: a run the CLI starts by itself. Its Stop hook (before its result) lists no
    // wakeup any more, so the wakeup ended as completed by then.
    until(&mut handle, &mut seen, "the woken run's end", |e| {
        matches!(e, AdapterEvent::TurnCompleted { .. })
    })
    .await;
    assert!(
        seen[turn_end + 1..]
            .iter()
            .any(|e| matches!(e, AdapterEvent::TurnStarted)),
        "the CLI started the run by itself"
    );
    let ended = latest(&seen);
    assert_eq!(
        (ended.state, ended.live),
        (aas_harness::BackgroundState::Completed, false),
        "{ended:?}"
    );
    let reply: String = seen[turn_end..]
        .iter()
        .filter_map(|e| match e {
            AdapterEvent::ItemCompleted {
                body: Some(ItemBody::AgentMessage { text }),
                ..
            } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert!(reply.contains("woke-up"), "{reply:?}");
    shut_down(handle, &supervisor).await;
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

// -------------------------------------------------------------------------------------------
// The paths of docs/adapters/claude.md §19 (steer, rename, status, side questions, moving work
// to the background, plan mode, fork at a turn, fast mode) against the real CLI.
// -------------------------------------------------------------------------------------------

/// Removes what a test created — its sessions ([`remove_transcripts`]) and the plan files its
/// plan mode wrote — when it ends, also when it fails half-way.
#[derive(Default)]
struct Cleanup {
    sessions: std::sync::Mutex<Vec<String>>,
    files: std::sync::Mutex<Vec<std::path::PathBuf>>,
}

impl Cleanup {
    fn session(&self, id: &str) {
        self.sessions.lock().unwrap().push(id.to_owned());
    }

    /// The plan files among the files the events show written (`~/.claude/plans/*.md`).
    fn plan_files(&self, events: &[AdapterEvent]) {
        for e in events {
            if let AdapterEvent::ItemStarted {
                body: ItemBody::FileChange { changes },
                ..
            } = e
            {
                for c in changes {
                    let path = std::path::PathBuf::from(&c.path);
                    let in_plans = path.parent().is_some_and(|d| {
                        d.ends_with(std::path::Path::new(".claude").join("plans"))
                    });
                    if in_plans {
                        self.files.lock().unwrap().push(path);
                    }
                }
            }
        }
    }
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        remove_transcripts(&self.sessions.lock().unwrap());
        for file in self.files.lock().unwrap().iter() {
            let _ = std::fs::remove_file(file);
        }
    }
}

fn haiku() -> ThreadSettings {
    ThreadSettings {
        model: Some("haiku".into()),
        effort: None,
        permission_mode: Some("default".into()),
    }
}

async fn start_new(
    adapter: &ClaudeAdapter,
    cwd: &std::path::Path,
    settings: ThreadSettings,
    options: StartOptions,
) -> aas_harness::SessionHandle {
    adapter
        .start_with(
            StartRequest {
                thread_id: ThreadId::generate(),
                cwd: cwd.to_path_buf(),
                settings,
                mode: StartMode::New,
            },
            options,
        )
        .await
        .unwrap()
}

/// Stops the session and waits for its `Exited`.
async fn stop(handle: &mut aas_harness::SessionHandle) {
    handle.control.shutdown(StopReason::User).await;
    while let Ok(Some(ev)) =
        tokio::time::timeout(Duration::from_secs(30), handle.events.recv()).await
    {
        if matches!(ev, AdapterEvent::Exited { .. }) {
            break;
        }
    }
}

fn row(sections: &[aas_harness::StatusSection], title: &str, label: &str) -> Option<String> {
    sections
        .iter()
        .find(|s| s.title == title)?
        .rows
        .iter()
        .find(|r| r.label == label)
        .map(|r| r.value.clone())
}

fn last_agent_message(events: &[AdapterEvent]) -> String {
    events
        .iter()
        .rev()
        .find_map(|e| match e {
            AdapterEvent::ItemCompleted {
                body: Some(ItemBody::AgentMessage { text }),
                ..
            } => Some(text.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

/// Status (with and without a session), a steer taken at a tool boundary, a side question during
/// the turn, a rename the CLI's status shows, and a foreground command moved to the background.
#[tokio::test]
#[ignore = "spends tokens; set AAS_LIVE_TESTS=1 and pass --ignored"]
async fn live_steer_side_question_rename_status_and_background() {
    if !live() {
        return;
    }
    let state = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let (adapter, supervisor) = adapter(state.path());
    let info = adapter.probe().await;
    assert!(info.capabilities.steer);
    let features = adapter.features();
    assert!(features.side_question && features.rename && features.move_to_background);

    // The status without a session: the plan's usage.
    let sections = adapter.status(work.path()).await.unwrap();
    assert!(
        sections.iter().any(|s| s.title == "Plan usage"),
        "{sections:?}"
    );

    let cleanup = Cleanup::default();
    let mut handle = start_new(&adapter, work.path(), haiku(), StartOptions::default()).await;
    cleanup.session(handle.native_session_id.as_deref().unwrap());
    let sections = handle.control.status().await.unwrap();
    assert!(
        row(&sections, "Session", "Version").is_some(),
        "{sections:?}"
    );

    // 1. A steer while the first command runs, and a side question beside the turn.
    handle
        .control
        .send(TurnInput::text(
            "Use the Bash tool to run exactly this command: sleep 12 && echo first-done\n\
             After it finishes, use the Bash tool again to run: echo second-step\n\
             Then reply with a one-line summary.",
        ))
        .await
        .unwrap();
    let mut seen = Vec::new();
    until(&mut handle, &mut seen, "the first command", |e| {
        matches!(
            e,
            AdapterEvent::ItemStarted {
                body: ItemBody::CommandExecution { .. },
                ..
            }
        )
    })
    .await;
    handle
        .control
        .steer_message(
            "steer-1",
            TurnInput::text(
                "Additional instruction from the user: at the very end of your final reply, add the word PINEAPPLE.",
            ),
        )
        .await
        .unwrap();
    let answer = handle
        .control
        .side_question("In one short sentence: what are you doing right now?")
        .await
        .unwrap();
    assert!(
        answer.answer.as_deref().is_some_and(|a| !a.is_empty()),
        "{answer:?}"
    );
    until(&mut handle, &mut seen, "the steered turn", |e| {
        matches!(e, AdapterEvent::TurnCompleted { .. })
    })
    .await;
    assert!(
        !seen
            .iter()
            .any(|e| matches!(e, AdapterEvent::SteerReturned { .. })),
        "the steer was taken at the tool boundary"
    );
    assert!(
        last_agent_message(&seen).contains("PINEAPPLE"),
        "{}",
        last_agent_message(&seen)
    );
    assert!(
        seen.iter()
            .any(|e| matches!(e, AdapterEvent::TurnAnchor { .. }))
    );

    // 2. A rename, which the CLI's own status shows.
    handle.control.rename("aas live rename").await.unwrap();
    let sections = handle.control.status().await.unwrap();
    assert_eq!(
        row(&sections, "Session", "Session name").as_deref(),
        Some("aas live rename"),
        "{sections:?}"
    );

    // 3. A foreground command moved to the background once the CLI registered its task.
    seen.clear();
    handle
        .control
        .send(TurnInput::text(
            "Use the Bash tool (in the foreground; do not set run_in_background) to run exactly: \
             ping -n 60 127.0.0.1 > /dev/null && echo bg-done\n\
             If the tool result says the command was moved to the background, reply immediately with just: moved. \
             Do not wait for it or check on it.",
        ))
        .await
        .unwrap();
    let movable = until(&mut handle, &mut seen, "backgroundable work", |e| {
        matches!(
            e,
            AdapterEvent::ItemBackgroundable {
                backgroundable: true,
                ..
            }
        )
    })
    .await;
    let AdapterEvent::ItemBackgroundable { key, .. } = movable else {
        unreachable!()
    };
    handle.control.move_to_background(&key).await.unwrap();
    until(&mut handle, &mut seen, "the moving turn", |e| {
        matches!(e, AdapterEvent::TurnCompleted { .. })
    })
    .await;
    assert!(seen.iter().any(|e| matches!(e, AdapterEvent::ItemCompleted { key: k, status: ItemStatus::Backgrounded, .. } if *k == key)));
    let shell = seen
        .iter()
        .filter_map(task_of)
        .rfind(|t| t.origin_item_key.as_deref() == Some(key.as_str()))
        .cloned()
        .expect("the moved command is a task");
    if shell.state == aas_harness::BackgroundState::Running {
        handle.control.stop_background(&shell.key).await.unwrap();
        until(&mut handle, &mut seen, "the stopped command", |e| {
            task_of(e).is_some_and(|t| t.key == shell.key && t.state.is_ended())
        })
        .await;
    }
    stop(&mut handle).await;
    assert_eq!(
        supervisor.running_count(),
        0,
        "no supervised process may remain"
    );
}

/// Plan mode (the plan presented for approval as a proposed plan, plan mode left after it),
/// turn anchors equal to the history's, forks at a turn and right before it, and a fork at an
/// anchor the session does not have (refused with the CLI's words).
#[tokio::test]
#[ignore = "spends tokens; set AAS_LIVE_TESTS=1 and pass --ignored"]
async fn live_plan_mode_anchors_and_forks_at_a_turn() {
    if !live() {
        return;
    }
    let state = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let (adapter, supervisor) = adapter(state.path());
    let info = adapter.probe().await;
    assert!(
        !info.permission_modes.iter().any(|m| m.id == "plan"),
        "plan mode is the thread's mode"
    );
    assert!(adapter.features().plan_mode.is_some() && adapter.features().fork_at_turn);
    let cleanup = Cleanup::default();

    let mut handle = start_new(
        &adapter,
        work.path(),
        haiku(),
        StartOptions {
            modes: ThreadModes {
                plan: true,
                fast: false,
            },
            ..StartOptions::default()
        },
    )
    .await;
    let native = handle.native_session_id.clone().unwrap();
    cleanup.session(&native);
    let mut seen = Vec::new();
    handle
        .control
        .send(TurnInput::text(
            "I want a file named hello.txt in the current directory containing exactly: hi\n\
             Make a one-line plan, present it with the ExitPlanMode tool, and after it is approved \
             create the file with the Write tool. Then reply DONE.",
        ))
        .await
        .unwrap();
    until(&mut handle, &mut seen, "the plan turn", |e| {
        matches!(e, AdapterEvent::TurnCompleted { .. })
    })
    .await;
    cleanup.plan_files(&seen);
    let plans: Vec<bool> = seen
        .iter()
        .filter_map(|e| match e {
            AdapterEvent::ModesReported { plan: Some(p), .. } => Some(*p),
            _ => None,
        })
        .collect();
    // Off after the handshake, on by the start's modes, off after the plan's approval.
    assert_eq!(plans, [false, true, false]);
    assert!(seen.iter().any(|e| matches!(e, AdapterEvent::ItemCompleted { body: Some(ItemBody::ProposedPlan { text }), status: ItemStatus::Completed, .. } if text.contains("hello.txt"))
        || matches!(e, AdapterEvent::ItemStarted { body: ItemBody::ProposedPlan { text }, .. } if text.contains("hello.txt"))));
    assert!(work.path().join("hello.txt").exists());

    // Three turns to fork at; their anchors.
    let mut anchors = Vec::new();
    for word in ["APPLE", "BANANA", "CHERRY"] {
        seen.clear();
        handle
            .control
            .send(TurnInput::text(format!(
                "Memory game: the next word is {word}. Reply with just OK. Do not use tools."
            )))
            .await
            .unwrap();
        until(&mut handle, &mut seen, "a memory turn", |e| {
            matches!(e, AdapterEvent::TurnCompleted { .. })
        })
        .await;
        let anchor = seen
            .iter()
            .rev()
            .find_map(|e| match e {
                AdapterEvent::TurnAnchor { anchor } => Some(anchor.clone()),
                _ => None,
            })
            .expect("the turn was anchored");
        anchors.push(anchor);
    }
    stop(&mut handle).await;

    // The history has the same anchors.
    let (history, history_anchors) = adapter
        .read_native_history_anchored(work.path(), &native)
        .await
        .unwrap();
    assert_eq!(history.turns.len(), 4);
    assert_eq!(
        history_anchors[1..].to_vec(),
        anchors.iter().cloned().map(Some).collect::<Vec<_>>()
    );

    let fork = |point: ForkPoint| {
        let adapter = &adapter;
        let native = native.clone();
        let cwd = work.path().to_path_buf();
        async move {
            adapter
                .start_with(
                    StartRequest {
                        thread_id: ThreadId::generate(),
                        cwd,
                        settings: haiku(),
                        mode: StartMode::Fork {
                            native_session_id: native,
                        },
                    },
                    StartOptions {
                        fork_at: Some(point),
                        ..StartOptions::default()
                    },
                )
                .await
        }
    };
    let ask = "List, numbered, every word of the memory game so far. Do not use tools.";
    // At BANANA: APPLE and BANANA.
    let mut at = fork(ForkPoint {
        anchor: anchors[1].clone(),
        before: false,
        previous: Some(anchors[0].clone()),
    })
    .await
    .unwrap();
    cleanup.session(at.native_session_id.as_deref().unwrap());
    at.control.send(TurnInput::text(ask)).await.unwrap();
    let events = until_turn_end(&mut at.events, |_| {}).await;
    let listed = last_agent_message(&events);
    assert!(
        listed.contains("BANANA") && !listed.contains("CHERRY"),
        "{listed}"
    );
    stop(&mut at).await;
    // Right before BANANA: APPLE only.
    let mut before = fork(ForkPoint {
        anchor: anchors[1].clone(),
        before: true,
        previous: Some(anchors[0].clone()),
    })
    .await
    .unwrap();
    cleanup.session(before.native_session_id.as_deref().unwrap());
    before.control.send(TurnInput::text(ask)).await.unwrap();
    let events = until_turn_end(&mut before.events, |_| {}).await;
    let listed = last_agent_message(&events);
    assert!(
        listed.contains("APPLE") && !listed.contains("BANANA"),
        "{listed}"
    );
    stop(&mut before).await;
    // An anchor the session does not have.
    let refused = fork(ForkPoint {
        anchor: serde_json::json!({"leafUuid": "11111111-2222-4333-8444-555555555555"}),
        before: false,
        previous: None,
    })
    .await;
    match refused {
        Err(e) => assert!(
            e.detail().contains("11111111-2222-4333-8444-555555555555"),
            "{e}"
        ),
        Ok(_) => panic!("the fork at an unknown anchor started"),
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        supervisor.running_count(),
        0,
        "no supervised process may remain"
    );
}

/// Fast mode for a model the CLI marks: on at the start, the CLI's state for it reported, off
/// again (one turn on opus). An account that cannot use fast mode (its extra usage is off) has
/// the CLI keep it off and say why (`fast_mode_disabled_reason`), which must reach the user as
/// the `fastModeDisabled` notice instead.
#[tokio::test]
#[ignore = "spends tokens; set AAS_LIVE_TESTS=1 and pass --ignored"]
async fn live_fast_mode() {
    if !live() {
        return;
    }
    let state = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let (adapter, supervisor) = adapter(state.path());
    adapter.probe().await;
    let fast = adapter.features().fast_mode_models;
    assert!(fast.iter().any(|m| m == "opus"), "{fast:?}");
    let mut handle = start_new(
        &adapter,
        work.path(),
        ThreadSettings {
            model: Some("opus".into()),
            ..haiku()
        },
        StartOptions {
            modes: ThreadModes {
                plan: false,
                fast: true,
            },
            ..StartOptions::default()
        },
    )
    .await;
    let cleanup = Cleanup::default();
    cleanup.session(handle.native_session_id.as_deref().unwrap());
    handle
        .control
        .send(TurnInput::text(
            "Reply with just the word OK. Do not use tools.",
        ))
        .await
        .unwrap();
    let mut seen = Vec::new();
    until(&mut handle, &mut seen, "the fast turn", |e| {
        matches!(e, AdapterEvent::TurnCompleted { .. })
    })
    .await;
    let on = seen
        .iter()
        .any(|e| matches!(e, AdapterEvent::ModesReported { fast_state: Some(s), .. } if s == "on"));
    let kept_off = seen.iter().find_map(|e| match e {
        AdapterEvent::Notice {
            message,
            code: Some(c),
            ..
        } if c == "fastModeDisabled" => Some(message.clone()),
        _ => None,
    });
    match (on, kept_off) {
        (true, None) => {}
        (false, Some(why)) => eprintln!("this account cannot use fast mode: {why}"),
        _ => panic!("fast mode neither on nor explained: {seen:?}"),
    }
    handle
        .control
        .apply_modes(&ThreadModes::default())
        .await
        .unwrap();
    stop(&mut handle).await;
    assert_eq!(supervisor.running_count(), 0);
}

/// A thread stored with the permission mode `plan` of earlier versions: the CLI starts in its own
/// default permission mode and enters plan mode, and leaving plan mode sets a permission mode the
/// CLI reports (never `plan` again). No model is called.
#[tokio::test]
#[ignore = "runs the installed claude CLI; set AAS_LIVE_TESTS=1 and pass --ignored"]
async fn live_a_legacy_plan_permission_mode_is_plan_mode() {
    if !live() {
        return;
    }
    let state = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let (adapter, supervisor) = adapter(state.path());
    adapter.probe().await;
    let mut handle = start_new(
        &adapter,
        work.path(),
        ThreadSettings {
            permission_mode: Some("plan".into()),
            ..haiku()
        },
        StartOptions::default(),
    )
    .await;
    let cleanup = Cleanup::default();
    cleanup.session(handle.native_session_id.as_deref().unwrap());
    let mut seen = Vec::new();
    until(&mut handle, &mut seen, "plan mode", |e| {
        matches!(
            e,
            AdapterEvent::ModesReported {
                plan: Some(true),
                ..
            }
        )
    })
    .await;
    handle
        .control
        .apply_modes(&ThreadModes::default())
        .await
        .unwrap();
    until(&mut handle, &mut seen, "plan mode left", |e| {
        matches!(
            e,
            AdapterEvent::ModesReported {
                plan: Some(false),
                ..
            }
        )
    })
    .await;
    let modes: Vec<&str> = seen
        .iter()
        .filter_map(|e| match e {
            AdapterEvent::SessionInfo {
                permission_mode: Some(m),
                ..
            } => Some(m.as_str()),
            _ => None,
        })
        .collect();
    assert!(!modes.is_empty() && !modes.contains(&"plan"), "{seen:?}");
    stop(&mut handle).await;
    assert_eq!(supervisor.running_count(), 0);
}
