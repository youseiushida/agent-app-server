//! Replays of Codex's background work (codex-cli 0.148.0, recorded with a scripted model; see
//! tests/fixtures/README.md): commands that outlive their turn, sub-agent threads, their
//! approvals after the parent's turn ended, and stopping each of them.

mod support;

use aas_harness::protocol::{
    BackgroundTaskKind, InteractionResolution, ItemBody, ItemStatus, NoticeLevel,
};
use aas_harness::{
    AdapterError, AdapterEvent, BackgroundState, BackgroundTaskInfo, ExpireReason, OutputUpdate,
    StartMode, ThreadSettings, TurnInput, TurnStatus,
};
use aas_supervisor::StopReason;
use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use support::Replay;

async fn start(script: &str) -> Replay {
    support::start(script, StartMode::New, ThreadSettings::default()).await
}

fn is_turn_completed(e: &AdapterEvent) -> bool {
    matches!(e, AdapterEvent::TurnCompleted { .. })
}

fn turn_status(e: &AdapterEvent) -> TurnStatus {
    match e {
        AdapterEvent::TurnCompleted { status, .. } => *status,
        other => panic!("expected TurnCompleted, got {other:?}"),
    }
}

/// Index of the first event matching `pred`.
fn position(r: &Replay, pred: impl Fn(&AdapterEvent) -> bool) -> usize {
    r.events
        .iter()
        .position(pred)
        .expect("the expected event was emitted")
}

fn task_event(e: &AdapterEvent, key: &str) -> Option<BackgroundTaskInfo> {
    match e {
        AdapterEvent::BackgroundTask { task } if task.key == key => Some((**task).clone()),
        _ => None,
    }
}

/// The last state emitted for task `key`.
fn last_task(r: &Replay, key: &str) -> BackgroundTaskInfo {
    r.events
        .iter()
        .rev()
        .find_map(|e| task_event(e, key))
        .unwrap_or_else(|| panic!("no state of task {key} was emitted"))
}

/// Reads events until task `key` is reported in `state`; returns that state.
async fn until_task(r: &mut Replay, key: &str, state: BackgroundState) -> BackgroundTaskInfo {
    let event = r
        .until(|e| task_event(e, key).is_some_and(|t| t.state == state))
        .await;
    task_event(&event, key).expect("matched")
}

/// Reads events until request `id` is asked; returns its background key and item key.
async fn until_request(r: &mut Replay, id: &str) -> (Option<String>, Option<String>) {
    match r
        .until(|e| matches!(e, AdapterEvent::InteractionRequested { request_id, .. } if request_id == id))
        .await
    {
        AdapterEvent::InteractionRequested {
            background_key,
            item_key,
            ..
        } => (background_key, item_key),
        _ => unreachable!(),
    }
}

fn accept() -> InteractionResolution {
    InteractionResolution::Approval {
        option_id: "accept".into(),
        feedback: None,
    }
}

async fn shutdown_cleanly(mut r: Replay) {
    let info = r.handle.control.shutdown(StopReason::Shutdown).await;
    assert_eq!(
        info.code,
        Some(0),
        "app-server exits by itself on stdin EOF"
    );
    let exited = r.until_closed().await;
    assert_eq!(exited, info);
    assert!(
        r.natives().is_empty(),
        "unexpected native events: {:?}",
        r.natives()
    );
    r.finish().await.expect("script fully replayed");
}

const TERM_A: &str = "mock_TERM_0";
const TERM_B: &str = "mock_TERM_1";

/// What was streamed as the output of task `key`, and at which event positions.
fn streamed(r: &Replay, key: &str) -> (String, Vec<usize>) {
    let mut text = String::new();
    let mut at = Vec::new();
    for (i, e) in r.events.iter().enumerate() {
        if let AdapterEvent::BackgroundOutput { key: k, output } = e
            && k == key
        {
            match output {
                OutputUpdate::Append(t) => text.push_str(t),
                OutputUpdate::Replace(t) => panic!("Codex streams deltas, not snapshots: {t:?}"),
            }
            at.push(i);
        }
    }
    (text, at)
}

/// Two commands outlive their turn: each becomes a background terminal (its task before its
/// item closes as backgrounded, both before the turn ends). A later turn and its interrupt leave
/// them running; the terminal stopped from the phone ends as stopped with Codex's exit code and
/// output, the other one completes by itself. Their output after the turn is not an item's: it
/// is streamed as the task's output (Codex's later `item/commandExecution/outputDelta`), while
/// the task runs.
#[tokio::test]
async fn commands_that_outlive_their_turn_become_background_terminals() {
    let mut r = start("bg_terminals.jsonl").await;
    r.handle
        .control
        .send(TurnInput::text("ROLE=TERM# start two long commands"))
        .await
        .unwrap();
    let first = r.until(is_turn_completed).await;
    assert_eq!(turn_status(&first), TurnStatus::Completed);
    let turn_end = r.events.len() - 1;
    for (key, title) in [
        (
            TERM_A,
            "for ($i=1; $i -le 9; $i++) { Write-Output \"TICK_A $i\"; Start-Sleep -Seconds 5 }; Write-Output BG_A_DONE",
        ),
        (
            TERM_B,
            "for ($i=1; $i -le 120; $i++) { Write-Output \"TICK_B $i\"; Start-Sleep -Seconds 5 }; Write-Output BG_B_DONE",
        ),
    ] {
        let task = position(&r, |e| task_event(e, key).is_some());
        let closed = position(
            &r,
            |e| matches!(e, AdapterEvent::ItemCompleted { key: k, body: None, status: ItemStatus::Backgrounded } if k == key),
        );
        assert!(
            task < closed && closed < turn_end,
            "{key}: {task} {closed} {turn_end}"
        );
        let info = last_task(&r, key);
        assert_eq!(
            (info.kind, info.title.as_str(), info.live, info.state),
            (
                BackgroundTaskKind::Shell,
                title,
                true,
                BackgroundState::Running
            )
        );
        assert_eq!(info.origin_item_key.as_deref(), Some(key));
        assert!(info.stoppable);
        assert_eq!((info.parent_key, info.result), (None, None));
    }

    // A later turn, interrupted: the terminals go on (Codex lists them again, unchanged).
    r.handle
        .control
        .send(TurnInput::text("ROLE=TERM2# Reply with exactly OK."))
        .await
        .unwrap();
    r.until(|e| matches!(e, AdapterEvent::TurnStarted)).await;
    r.handle.control.interrupt().await.unwrap();
    let second = r.until(is_turn_completed).await;
    assert_eq!(turn_status(&second), TurnStatus::Interrupted);
    assert!(
        r.events[turn_end + 1..]
            .iter()
            .all(|e| !matches!(e, AdapterEvent::BackgroundTask { .. })),
        "nothing about the terminals changed"
    );

    // Stopped from the phone: Codex ends the process as a failed command (-1).
    r.handle.control.stop_background(TERM_B).await.unwrap();
    let b = until_task(&mut r, TERM_B, BackgroundState::Stopped).await;
    assert!(!b.live);
    let result = b.result.expect("result");
    assert_eq!(result.exit_code, Some(-1));
    assert_eq!(
        result.output.as_deref(),
        Some("TICK_B 1\r\nTICK_B 2\r\nTICK_B 3\r\nTICK_B 4\r\n")
    );
    assert_eq!(b.usage.and_then(|u| u.duration_ms), Some(18029));
    assert!(
        last_task(&r, TERM_A).live,
        "the other terminal is still listed"
    );

    // The other one finishes by itself.
    let a = until_task(&mut r, TERM_A, BackgroundState::Completed).await;
    assert!(!a.live);
    let result = a.result.expect("result");
    assert_eq!(result.exit_code, Some(0));
    assert!(
        result
            .output
            .unwrap()
            .ends_with("TICK_A 9\r\nBG_A_DONE\r\n")
    );

    // Their output after the turn ended was not forwarded as item output, but as the tasks'
    // (what came before they went on in the background was the items').
    assert!(
        r.events[turn_end..].iter().all(|e| !matches!(
            e,
            AdapterEvent::ItemDelta { key, .. } if key == TERM_A || key == TERM_B
        )),
        "no deltas of the backgrounded items"
    );
    let (a_out, a_at) = streamed(&r, TERM_A);
    assert_eq!(
        a_out,
        "TICK_A 6\r\nTICK_A 7\r\nTICK_A 8\r\nTICK_A 9\r\nBG_A_DONE\r\n"
    );
    // TICK_B 3 came after the turn's `turn/completed`, which the adapter answers by listing the
    // terminals before it reads on: from then on the output is the terminal's.
    let (b_out, b_at) = streamed(&r, TERM_B);
    assert_eq!(b_out, "TICK_B 3\r\nTICK_B 4\r\n");
    for (key, at) in [(TERM_A, &a_at), (TERM_B, &b_at)] {
        let closed = position(
            &r,
            |e| matches!(e, AdapterEvent::ItemCompleted { key: k, status: ItemStatus::Backgrounded, .. } if k == key),
        );
        let ended = r
            .events
            .iter()
            .position(|e| task_event(e, key).is_some_and(|t| t.state.is_ended()))
            .expect("ended");
        assert!(
            at.iter().all(|i| closed < *i && *i < ended),
            "{key}: streamed between going to the background ({closed}) and the end ({ended}): {at:?}"
        );
    }
    // An ended task, or one this session does not know, cannot be stopped.
    assert!(r.handle.control.stop_background(TERM_A).await.is_err());
    assert!(r.handle.control.stop_background("nope").await.is_err());
    shutdown_cleanly(r).await;
}

/// A Codex without the experimental API refuses the terminal list: today's behaviour (the
/// engine closes the open commands with the turn), one notice, and the late completion of a
/// command the turn closed is not reported again.
#[tokio::test]
async fn without_the_terminal_list_open_commands_close_with_the_turn() {
    let mut r = support::start_entries(
        support::script("bg_terminals_unlisted.jsonl"),
        StartMode::New,
        ThreadSettings::default(),
        support::policy(),
    )
    .await;
    r.handle
        .control
        .send(TurnInput::text("ROLE=TERM# start two long commands"))
        .await
        .unwrap();
    r.until(is_turn_completed).await;
    r.handle
        .control
        .send(TurnInput::text("ROLE=TERM2# Reply with exactly OK."))
        .await
        .unwrap();
    r.until(|e| matches!(e, AdapterEvent::TurnStarted)).await;
    r.handle.control.interrupt().await.unwrap();
    r.until(is_turn_completed).await;
    r.handle.control.shutdown(StopReason::Shutdown).await;
    r.until_closed().await;
    let notices: Vec<&AdapterEvent> = r
        .events
        .iter()
        .filter(|e| {
            matches!(e, AdapterEvent::Notice { code: Some(c), level: NoticeLevel::Warning, message }
                if c == "backgroundTerminalsUnavailable" && message.contains("requires experimentalApi"))
        })
        .collect();
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert!(
        !r.events
            .iter()
            .any(|e| matches!(e, AdapterEvent::BackgroundTask { .. })),
        "no task without Codex's list"
    );
    assert!(!r.events.iter().any(|e| matches!(
        e,
        AdapterEvent::ItemCompleted { key, .. } if key == TERM_A || key == TERM_B
    )));
    assert!(r.natives().is_empty(), "{:?}", r.natives());
    r.finish().await.expect("script fully replayed");
}

const APPROVER: &str = "01a0e7e0-e598-7210-bc0d-0cfc936547f2";
const SLEEPER: &str = "01a0e7e0-e780-7430-bb94-54379cde1416";
const PENDING: &str = "01a0e7e0-e98f-71e1-86d9-e8c8b999a1c2";

/// v2 sub-agents are background tasks of their own runs: spawned by the parent's turn (the
/// spawning item closes as backgrounded), they ask for approvals after the parent's turn ended
/// (answered through the task), they are stopped one by one (their commands going on as
/// terminals of their own), and one completes with its final answer as the summary.
#[tokio::test]
async fn sub_agents_run_ask_and_stop_as_background_tasks() {
    let mut r = start("bg_subagents_v2.jsonl").await;
    r.handle
        .control
        .send(TurnInput::text("ROLE=PARENT_SUB# spawn three sub-agents"))
        .await
        .unwrap();
    let first = r.until(is_turn_completed).await;
    assert_eq!(turn_status(&first), TurnStatus::Completed);
    let turn_end = r.events.len() - 1;
    for (n, (child, path)) in [
        (APPROVER, "/root/approver"),
        (SLEEPER, "/root/sleeper"),
        (PENDING, "/root/pending"),
    ]
    .into_iter()
    .enumerate()
    {
        let spawn = format!("mock_PARENT_SUB_{n}");
        let started = position(
            &r,
            |e| matches!(e, AdapterEvent::ItemStarted { key, body: ItemBody::ToolCall { .. } } if *key == spawn),
        );
        let task = position(&r, |e| task_event(e, child).is_some());
        let closed = position(
            &r,
            |e| matches!(e, AdapterEvent::ItemCompleted { key, status: ItemStatus::Backgrounded, .. } if *key == spawn),
        );
        assert!(started < task && task < closed && closed < turn_end);
        let info = last_task(&r, child);
        assert_eq!(
            (info.kind, info.title.as_str(), info.state, info.live),
            (
                BackgroundTaskKind::Agent,
                path,
                BackgroundState::Running,
                true
            )
        );
        assert_eq!(info.origin_item_key.as_deref(), Some(spawn.as_str()));
        assert!(info.stoppable);
    }

    // Their approvals arrive after the parent's turn ended and belong to them.
    for (id, child) in [("0", SLEEPER), ("2", PENDING), ("1", APPROVER)] {
        let (background_key, item_key) = until_request(&mut r, id).await;
        assert_eq!(background_key.as_deref(), Some(child));
        assert_eq!(
            item_key, None,
            "a sub-agent's item is not an item of the session"
        );
    }
    r.handle.control.respond("0", &accept()).await.unwrap();
    r.handle.control.respond("1", &accept()).await.unwrap();

    // The sleeper is stopped: its turn is interrupted, its command goes on as its terminal.
    r.handle.control.stop_background(SLEEPER).await.unwrap();
    let sleeper = until_task(&mut r, SLEEPER, BackgroundState::Stopped).await;
    assert!(!sleeper.live);
    let terminal_key = format!("{SLEEPER}:mock_sleeper_0");
    let terminal = last_task(&r, &terminal_key);
    assert_eq!(
        (
            terminal.kind,
            terminal.title.as_str(),
            terminal.live,
            terminal.parent_key.as_deref()
        ),
        (
            BackgroundTaskKind::Shell,
            "Start-Sleep -Seconds 300; Write-Output SLEEPER_OK",
            true,
            Some(SLEEPER)
        )
    );
    assert_eq!(terminal.origin_item_key, None);
    r.handle
        .control
        .stop_background(&terminal_key)
        .await
        .unwrap();
    let terminal = until_task(&mut r, &terminal_key, BackgroundState::Stopped).await;
    assert_eq!(terminal.result.unwrap().exit_code, Some(-1));

    // The pending one is stopped while its approval waits: Codex withdraws the request, and
    // the engine's later answer to it finds nothing to answer.
    r.handle.control.stop_background(PENDING).await.unwrap();
    until_task(&mut r, PENDING, BackgroundState::Stopped).await;
    r.until(
        |e| matches!(e, AdapterEvent::InteractionWithdrawn { request_id } if request_id == "2"),
    )
    .await;
    assert_eq!(
        r.handle
            .control
            .expire_request("2", ExpireReason::TaskEnded)
            .await,
        Err(AdapterError::UnknownRequest("2".into()))
    );

    // The approver asks again long after the parent's turn, and finishes.
    let (background_key, _) = until_request(&mut r, "3").await;
    assert_eq!(background_key.as_deref(), Some(APPROVER));
    r.handle.control.respond("3", &accept()).await.unwrap();
    let done = until_task(&mut r, APPROVER, BackgroundState::Completed).await;
    assert_eq!(
        done.result.and_then(|r| r.summary).as_deref(),
        Some("APPROVER_DONE")
    );
    assert_eq!(done.usage.and_then(|u| u.total_tokens), Some(3030));
    let progress = done.progress.expect("progress");
    assert_eq!(
        (progress.tool_uses, progress.last_tool_name.as_deref()),
        (Some(2), Some("commandExecution"))
    );
    // A stopped sub-agent cannot be stopped again.
    assert!(r.handle.control.stop_background(SLEEPER).await.is_err());

    // The parent goes on as usual.
    r.handle
        .control
        .send(TurnInput::text("ROLE=PARENT_FOLLOWUP# reply"))
        .await
        .unwrap();
    let last = r.until(is_turn_completed).await;
    assert_eq!(turn_status(&last), TurnStatus::Completed);
    shutdown_cleanly(r).await;
}

const V1_CHILD: &str = "01a0e7e9-5751-7f40-a0a9-fe159bf7683b";

/// v1: the spawn item names the sub-agent when it completes (its task text is the title). The
/// parent's interrupt does not reach it; it is stopped on its own, and its command is left as
/// its terminal.
#[tokio::test]
async fn a_v1_sub_agent_outlives_the_parents_interrupt() {
    let mut r = start("bg_subagent_v1_interrupt.jsonl").await;
    r.handle
        .control
        .send(TurnInput::text(
            "ROLE=PARENT_WAIT# spawn a sub-agent and wait",
        ))
        .await
        .unwrap();
    // The parent waits for the sub-agent (a `wait` item) and is interrupted.
    r.until(|e| matches!(e, AdapterEvent::ItemStarted { key, .. } if key == "mock_PARENT_WAIT_1"))
        .await;
    r.handle.control.interrupt().await.unwrap();
    let parent = r.until(is_turn_completed).await;
    assert_eq!(turn_status(&parent), TurnStatus::Interrupted);
    let child = last_task(&r, V1_CHILD);
    assert_eq!(
        (child.kind, child.state, child.live),
        (BackgroundTaskKind::Agent, BackgroundState::Running, true)
    );
    assert!(
        child
            .title
            .starts_with("ROLE=sleeper# [sleeper] Run `Start-Sleep"),
        "{}",
        child.title
    );
    assert_eq!(child.origin_item_key.as_deref(), Some("mock_PARENT_WAIT_0"));
    assert!(r.events.iter().any(|e| matches!(
        e,
        AdapterEvent::ItemCompleted { key, status: ItemStatus::Backgrounded, .. } if key == "mock_PARENT_WAIT_0"
    )));

    r.handle.control.stop_background(V1_CHILD).await.unwrap();
    until_task(&mut r, V1_CHILD, BackgroundState::Stopped).await;
    let terminal_key = format!("{V1_CHILD}:mock_sleeper_0");
    assert!(last_task(&r, &terminal_key).live);
    r.handle
        .control
        .stop_background(&terminal_key)
        .await
        .unwrap();
    let terminal = until_task(&mut r, &terminal_key, BackgroundState::Stopped).await;
    assert!(!terminal.live);
    shutdown_cleanly(r).await;
}

/// Sub-agents first seen through their own messages (their spawning item not seen) are looked
/// up with `thread/read`: a thread whose parent is the session's is a sub-agent (titled with its
/// agent path); a thread that is not part of the session's tree is not, and its request is
/// refused rather than left waiting.
#[tokio::test]
async fn threads_seen_first_through_their_messages_are_looked_up() {
    let mut r = start("bg_subagent_unannounced.jsonl").await;
    r.handle
        .control
        .send(TurnInput::text("ROLE=PARENT_SUB# spawn three sub-agents"))
        .await
        .unwrap();
    r.until(is_turn_completed).await;
    for (id, child, path) in [
        ("0", SLEEPER, "/root/sleeper"),
        ("1", APPROVER, "/root/approver"),
    ] {
        let (background_key, _) = until_request(&mut r, id).await;
        assert_eq!(background_key.as_deref(), Some(child));
        let info = last_task(&r, child);
        assert_eq!(info.title, path);
        assert_eq!(info.origin_item_key, None);
    }
    assert!(
        !r.events.iter().any(|e| task_event(e, PENDING).is_some()),
        "the thread outside the session is not a task"
    );
    assert!(!r.events.iter().any(|e| matches!(
        e,
        AdapterEvent::InteractionRequested { request_id, .. } if request_id == "2"
    )));
    r.handle.control.respond("0", &accept()).await.unwrap();
    r.handle.control.respond("1", &accept()).await.unwrap();
    shutdown_cleanly(r).await;
}

/// A turn the agent starts by itself (Codex's goal continuation starts turns without
/// `turn/start`): `send` meanwhile is refused with `TurnInProgress`, after that turn's
/// `TurnStarted`; once it completes, the input is sent as usual.
#[tokio::test]
async fn a_turn_the_agent_started_itself_defers_the_users_input() {
    // The recorded basic turn, with a turn of the agent's own before it (the same messages
    // under another turn id).
    let mut entries = support::script("main_basic_turn.jsonl");
    let at = entries
        .iter()
        .position(|e| e["c"]["method"] == "turn/start")
        .expect("the script starts a turn");
    let own_turn: Vec<Value> = entries[at..]
        .iter()
        .filter(|e| {
            let method = e["s"]["method"].as_str().unwrap_or_default();
            matches!(method, "turn/started" | "turn/completed")
        })
        .map(|e| {
            let mut e = e.clone();
            e["s"]["params"]["turn"]["id"] = json!("goal-turn");
            e
        })
        .collect();
    assert_eq!(own_turn.len(), 2);
    entries.insert(at, own_turn[1].clone());
    entries.insert(at, json!({"c": {"method": "turn/interrupt", "recId": "goal", "params": {"turnId": "goal-turn"}}}));
    entries.insert(
        at + 1,
        json!({"s": {"id": "goal", "result": {}}, "respondsTo": "goal"}),
    );
    entries.insert(at, own_turn[0].clone());
    let mut r = support::start_entries(
        entries,
        StartMode::New,
        ThreadSettings::default(),
        support::policy(),
    )
    .await;
    r.until(|e| matches!(e, AdapterEvent::TurnStarted)).await;
    let refused = r.handle.control.send(TurnInput::text("mine")).await;
    assert_eq!(refused, Err(AdapterError::TurnInProgress));
    // The agent's turn can be interrupted like any other.
    r.handle.control.interrupt().await.unwrap();
    r.until(is_turn_completed).await;
    r.handle
        .control
        .send(TurnInput::text("Reply with exactly: OK"))
        .await
        .unwrap();
    let mine = r.until(is_turn_completed).await;
    assert_eq!(turn_status(&mine), TurnStatus::Completed);
    // Codex says nothing about why it started its own turn: no trigger.
    assert!(r.events.iter().all(|e| !matches!(
        e,
        AdapterEvent::TurnCompleted {
            trigger: Some(_),
            ..
        }
    )));
    shutdown_cleanly(r).await;
}
