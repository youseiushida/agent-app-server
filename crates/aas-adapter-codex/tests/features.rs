//! Replays of the port's features against recordings of codex-cli 0.148.0 (round 2; how the
//! scripts were made is in tests/fixtures/README.md): plan mode, forks at a turn, held threads,
//! renames, goals, the inline review, fast mode and the status.

mod support;

use aas_harness::protocol::{ItemBody, ItemStatus, NoticeLevel};
use aas_harness::{
    AdapterError, AdapterEvent, ForkPoint, SettingsApplied, StartMode, StartOptions, StatusSection,
    ThreadModes, ThreadSettings, TurnInput, TurnStatus,
};
use aas_supervisor::StopReason;
use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use support::Replay;

fn is_turn_completed(e: &AdapterEvent) -> bool {
    matches!(e, AdapterEvent::TurnCompleted { .. })
}

fn completed_status(e: &AdapterEvent) -> TurnStatus {
    match e {
        AdapterEvent::TurnCompleted { status, .. } => *status,
        other => panic!("expected TurnCompleted, got {other:?}"),
    }
}

fn anchors(events: &[AdapterEvent]) -> Vec<Value> {
    events
        .iter()
        .filter_map(|e| match e {
            AdapterEvent::TurnAnchor { anchor } => Some(anchor.clone()),
            _ => None,
        })
        .collect()
}

fn notices(events: &[AdapterEvent]) -> Vec<(NoticeLevel, String, String)> {
    events
        .iter()
        .filter_map(|e| match e {
            AdapterEvent::Notice {
                level,
                message,
                code,
            } => Some((*level, message.clone(), code.clone().unwrap_or_default())),
            _ => None,
        })
        .collect()
}

fn modes(plan: bool, fast: bool) -> StartOptions {
    StartOptions {
        modes: ThreadModes { plan, fast },
        ..StartOptions::default()
    }
}

fn row<'a>(sections: &'a [StatusSection], title: &str, label: &str) -> Option<&'a str> {
    sections
        .iter()
        .find(|s| s.title == title)?
        .rows
        .iter()
        .find(|r| r.label == label)
        .map(|r| r.value.as_str())
}

/// Sends `text` and reads the events up to its turn's end.
async fn turn(r: &mut Replay, text: &str) -> Vec<AdapterEvent> {
    let from = r.events.len();
    r.handle.control.send(TurnInput::text(text)).await.unwrap();
    r.until(is_turn_completed).await;
    r.events[from..].to_vec()
}

async fn shutdown_cleanly(mut r: Replay) {
    let info = r.handle.control.shutdown(StopReason::Shutdown).await;
    assert_eq!(info.code, Some(0));
    r.until_closed().await;
    assert!(
        r.natives().is_empty(),
        "unexpected native events: {:?}",
        r.natives()
    );
    r.finish().await.expect("script fully replayed");
}

// ----- plan mode ------------------------------------------------------------------------------

#[tokio::test]
async fn plan_mode_is_a_collaboration_mode_and_its_plan_an_item() {
    let mut r = support::start_with(
        "plan_mode.jsonl",
        StartMode::New,
        ThreadSettings::default(),
        modes(true, false),
        &[],
    )
    .await;
    // The first turn asks for plan mode (the script checks `collaborationMode`).
    let events = turn(
        &mut r,
        "ROLE=PLAN# Plan how to make hello.txt say \"Hello, world\". Do not change files yet.",
    )
    .await;
    assert_eq!(
        completed_status(events.last().unwrap()),
        TurnStatus::Completed
    );
    // Codex's `thread/settings/updated` says plan mode is on.
    assert!(events.iter().any(|e| matches!(
        e,
        AdapterEvent::ModesReported {
            plan: Some(true),
            fast_state: None
        }
    )));
    assert_eq!(
        anchors(&events),
        [json!({"turnId": "01a0e941-d48b-7160-9529-c6e69b7e43bf"})]
    );
    let plan_key = "01a0e941-d48b-7160-9529-c6e69b7e43bf-plan";
    let plan_text = "# Rename greeting\n\n1. Change `hello.txt` so it says `Hello, world`.\n2. Verify the file content.\n";
    assert!(events.iter().any(|e| matches!(e,
        AdapterEvent::ItemStarted { key, body: ItemBody::ProposedPlan { text } } if key == plan_key && text.is_empty())));
    assert_eq!(r.deltas_of(plan_key), plan_text);
    assert!(events.iter().any(|e| matches!(e,
        AdapterEvent::ItemCompleted { key, body: Some(ItemBody::ProposedPlan { text }), status: ItemStatus::Completed }
            if key == plan_key && text == plan_text)));
    // The agent message keeps the text around the plan; Codex removed the block from it.
    assert!(events.iter().any(|e| matches!(e,
        AdapterEvent::ItemCompleted { key, body: Some(ItemBody::AgentMessage { text }), .. }
            if key == "msg_mock_1" && text == "I read the task and prepared a plan.\n\n\nSwitch to Default mode to implement it.")));

    // Codex keeps the mode: the next turn does not send it again.
    turn(
        &mut r,
        "ROLE=SAY_STICKY# (no collaborationMode on this turn)",
    )
    .await;

    // "Implement the plan": plan mode off, Codex's own text, in default mode (the script checks
    // `collaborationMode` and that no top-level effort is sent with it).
    assert_eq!(
        r.handle
            .control
            .apply_modes(&ThreadModes {
                plan: false,
                fast: false
            })
            .await
            .unwrap(),
        SettingsApplied::Live
    );
    let events = turn(&mut r, aas_adapter_codex::IMPLEMENT_PLAN_PROMPT).await;
    assert!(events.iter().any(|e| matches!(
        e,
        AdapterEvent::ModesReported {
            plan: Some(false),
            ..
        }
    )));
    shutdown_cleanly(r).await;
}

#[tokio::test]
async fn plan_mode_is_stated_again_after_a_resume() {
    let mut r = support::start_with(
        "resume_plan.jsonl",
        StartMode::Resume {
            native_session_id: "01a0e947-bf77-79e1-9c6a-26bb1ef17763".into(),
        },
        ThreadSettings::default(),
        modes(true, false),
        &[],
    )
    .await;
    // Codex does not restore the mode on resume: the first turn states it.
    let events = turn(
        &mut r,
        "ROLE=SAY_AFTERRESUME# turn after resume, no collaborationMode",
    )
    .await;
    assert_eq!(
        completed_status(events.last().unwrap()),
        TurnStatus::Completed
    );
    shutdown_cleanly(r).await;
}

// ----- forks at a turn, held threads -----------------------------------------------------------

const SOURCE: &str = "01a0e93f-b44a-7780-9cf4-c8ca66db79c3";
const T2: &str = "01a0e93f-b799-7e01-b5a5-04d9014e67f4";

fn fork_at(turn: &str, before: bool) -> StartOptions {
    StartOptions {
        fork_at: Some(ForkPoint {
            anchor: json!({ "turnId": turn }),
            before,
            previous: None,
        }),
        ..StartOptions::default()
    }
}

fn fork_of_source() -> StartMode {
    StartMode::Fork {
        native_session_id: SOURCE.into(),
    }
}

#[tokio::test]
async fn a_fork_at_a_turn_keeps_that_turn() {
    let mut r = support::start_with(
        "fork_at_turn.jsonl",
        fork_of_source(),
        ThreadSettings::default(),
        fork_at(T2, false),
        &[],
    )
    .await;
    assert_eq!(
        r.handle.native_session_id.as_deref(),
        Some("01a0e93f-b968-7a21-ad45-edf9a025a449")
    );
    // The first turn on the fork states the collaboration mode (Codex does not carry it over).
    let events = turn(&mut r, "ROLE=SAY_F1# prompt on the fork").await;
    assert_eq!(
        anchors(&events),
        [json!({"turnId": "01a0e93f-d5a6-7743-ae1d-8c0134f23bf1"})]
    );
    shutdown_cleanly(r).await;
}

#[tokio::test]
async fn a_fork_before_a_turn_asks_for_what_precedes_it() {
    let r = support::start_with(
        "fork_before_turn.jsonl",
        fork_of_source(),
        ThreadSettings::default(),
        fork_at(T2, true),
        &[],
    )
    .await;
    assert_eq!(
        r.handle.native_session_id.as_deref(),
        Some("01a0e93f-bd48-7f80-b2c2-ed1cc6486f78")
    );
    shutdown_cleanly(r).await;
}

#[tokio::test]
async fn a_fork_codex_refuses_fails_the_start_with_its_words() {
    let (error, verdict) = support::start_failing(
        "fork_unknown_turn.jsonl",
        fork_of_source(),
        fork_at("019999ff-0000-7000-8000-000000000000", false),
    )
    .await;
    verdict.expect("script fully replayed");
    assert!(matches!(error, AdapterError::Harness(_)), "{error:?}");
    assert_eq!(
        error.detail(),
        "thread/fork: lastTurnId '019999ff-0000-7000-8000-000000000000' was not found in the source thread"
    );
}

#[tokio::test]
async fn a_fork_point_that_is_not_a_codex_anchor_is_refused_before_codex_is_asked() {
    // Only the handshake: nothing is sent for the fork.
    let entries: Vec<Value> = support::script("fork_unknown_turn.jsonl")
        .into_iter()
        .take(3)
        .collect();
    let options = StartOptions {
        fork_at: Some(ForkPoint {
            anchor: json!({ "uuid": "x" }),
            before: false,
            previous: None,
        }),
        ..StartOptions::default()
    };
    let Err((error, runner)) = support::try_start_entries(
        entries,
        fork_of_source(),
        ThreadSettings::default(),
        options,
        &[],
        support::policy(),
    )
    .await
    else {
        panic!("the start succeeded")
    };
    assert!(
        error.detail().contains("is not the anchor of a Codex turn"),
        "{error}"
    );
    runner.await.unwrap().expect("script fully replayed");
}

#[tokio::test]
async fn resuming_a_thread_another_app_server_writes_fails_with_codexs_words() {
    let (error, verdict) = support::start_failing(
        "resume_active_writer.jsonl",
        StartMode::Resume {
            native_session_id: "01a0e940-cbba-7b32-bd56-b0cef3dad5f5".into(),
        },
        StartOptions::default(),
    )
    .await;
    verdict.expect("script fully replayed");
    assert!(matches!(error, AdapterError::Harness(_)), "{error:?}");
    assert_eq!(
        error.detail(),
        "thread/resume: thread 01a0e940-cbba-7b32-bd56-b0cef3dad5f5 already has an active writer"
    );
    // The kind's prefix appears once.
    assert_eq!(
        error
            .to_string()
            .matches("the harness reported an error")
            .count(),
        1
    );
    // Such a thread can still be forked (recorded): the client may offer a fork.
    let features = aas_adapter_codex::testing::features_for_models(json!([])).unwrap();
    assert!(features.fork_while_held && features.fork_at_turn);
}

#[tokio::test]
async fn a_resumed_thread_reports_the_name_it_got_elsewhere() {
    let mut r = support::start_with(
        "resume_named.jsonl",
        StartMode::Resume {
            native_session_id: "01a0e940-cbba-7b32-bd56-b0cef3dad5f5".into(),
        },
        ThreadSettings::default(),
        StartOptions::default(),
        &[],
    )
    .await;
    let title = r
        .until(|e| matches!(e, AdapterEvent::SessionTitle { .. }))
        .await;
    assert_eq!(
        title,
        AdapterEvent::SessionTitle {
            title: "Renamed by B".into()
        }
    );
    // The first turn after a resume states the collaboration mode (default here).
    let events = turn(&mut r, "ROLE=SAY_B2# prompt after B resumed").await;
    assert_eq!(
        completed_status(events.last().unwrap()),
        TurnStatus::Completed
    );
    shutdown_cleanly(r).await;
}

// ----- renames ----------------------------------------------------------------------------------

#[tokio::test]
async fn renames_reach_codex_and_its_echo_is_the_title() {
    let mut r = support::start_with(
        "rename.jsonl",
        StartMode::New,
        ThreadSettings::default(),
        StartOptions::default(),
        &[],
    )
    .await;
    turn(&mut r, "ROLE=SAY_N1# first prompt").await;
    r.handle.control.rename("  Padded name  ").await.unwrap();
    // Codex trims the name and echoes it.
    assert_eq!(
        r.until(|e| matches!(e, AdapterEvent::SessionTitle { .. }))
            .await,
        AdapterEvent::SessionTitle {
            title: "Padded name".into()
        }
    );
    let error = r.handle.control.rename("").await.unwrap_err();
    assert_eq!(
        error.detail(),
        "thread/name/set: thread name must not be empty"
    );
    r.handle.control.rename("Second name").await.unwrap();
    assert_eq!(
        r.until(|e| matches!(e, AdapterEvent::SessionTitle { .. }))
            .await,
        AdapterEvent::SessionTitle {
            title: "Second name".into()
        }
    );
    shutdown_cleanly(r).await;
}

// ----- /init ------------------------------------------------------------------------------------

#[tokio::test]
async fn init_sends_codexs_own_prompt() {
    let mut r = support::start_with(
        "init_command.jsonl",
        StartMode::New,
        ThreadSettings::default(),
        StartOptions::default(),
        &[],
    )
    .await;
    // The script expects the prompt read from the codex-cli 0.148.0 binary.
    let events = turn(&mut r, "/init").await;
    assert_eq!(
        completed_status(events.last().unwrap()),
        TurnStatus::Completed
    );
    assert_eq!(anchors(&events).len(), 1);
    shutdown_cleanly(r).await;
}

// ----- inline review ----------------------------------------------------------------------------

const REVIEW_TURN: &str = "01a0e945-6aab-74c3-8920-c9e8bfd57d26";
const REVIEW: &str = "/review ROLE=REVIEW# Review hello.txt for wording problems (inline).";

#[tokio::test]
async fn an_inline_review_shows_codexs_rendered_findings() {
    let mut r = support::start_with(
        "review_inline.jsonl",
        StartMode::New,
        ThreadSettings::default(),
        StartOptions::default(),
        &[],
    )
    .await;
    let events = turn(&mut r, REVIEW).await;
    assert_eq!(
        completed_status(events.last().unwrap()),
        TurnStatus::Completed
    );
    // One turn: the review's (its reviewer's turn under another id is not the session's).
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, AdapterEvent::TurnStarted))
            .count(),
        1
    );
    assert_eq!(anchors(&events), [json!({ "turnId": REVIEW_TURN })]);
    // The reviewer's JSON (an agent message Codex never completes) is not shown.
    assert!(
        !events.iter().any(|e| matches!(e,
            AdapterEvent::ItemStarted { key, .. } | AdapterEvent::ItemDelta { key, .. } | AdapterEvent::ItemCompleted { key, .. }
                if key == "msg_mock_2")),
        "{events:#?}"
    );
    let messages: Vec<String> = events
        .iter()
        .filter_map(|e| match e {
            AdapterEvent::ItemCompleted {
                body: Some(ItemBody::Notice { message, .. }),
                ..
            } => Some(message.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        messages,
        [
            "Review started: ROLE=REVIEW# Review hello.txt for wording problems (inline).",
            "Review finished"
        ]
    );
    assert!(events.iter().any(|e| matches!(e,
        AdapterEvent::ItemCompleted { key, body: Some(ItemBody::AgentMessage { text }), .. }
            if key == "msg_01a0e945-6ba0-7b10-96c6-db45e27a27b5"
                && text.starts_with("One wording problem in hello.txt.\n\nReview comment:\n\n- [P2] Greeting is missing a comma"))));
    shutdown_cleanly(r).await;
}

#[tokio::test]
async fn an_inline_review_is_interrupted_as_the_review_turn() {
    let mut r = support::start_with(
        "review_interrupt.jsonl",
        StartMode::New,
        ThreadSettings::default(),
        StartOptions::default(),
        &[],
    )
    .await;
    r.handle
        .control
        .send(TurnInput::text(REVIEW))
        .await
        .unwrap();
    // After the reviewer's turn started (the script's sync point follows it).
    r.until(
        |e| matches!(e, AdapterEvent::Notice { message, .. } if message == "replay sync point"),
    )
    .await;
    // The script checks that the review turn is the one interrupted.
    r.handle.control.interrupt().await.unwrap();
    let done = r.until(is_turn_completed).await;
    assert_eq!(completed_status(&done), TurnStatus::Interrupted);
    shutdown_cleanly(r).await;
}

// ----- fast mode and the status ------------------------------------------------------------------

const FAST_TIERS: &[(&str, &str, &str)] = &[("gpt-5.6-sol", "priority", "Fast")];

#[tokio::test]
async fn fast_mode_is_the_models_service_tier_and_the_status_shows_codexs_limits() {
    let settings = ThreadSettings {
        model: Some("gpt-5.6-sol".into()),
        ..ThreadSettings::default()
    };
    let mut r = support::start_with(
        "service_tier.jsonl",
        StartMode::New,
        settings.clone(),
        modes(false, true),
        FAST_TIERS,
    )
    .await;
    // The thread starts with the tier (the script checks `serviceTier`), and Codex says so.
    let reported = r
        .until(|e| matches!(e, AdapterEvent::ModesReported { .. }))
        .await;
    assert_eq!(
        reported,
        AdapterEvent::ModesReported {
            plan: None,
            fast_state: Some("Fast".into())
        }
    );
    // Codex keeps the tier: the turn does not send it.
    turn(&mut r, "ROLE=SAY_T1# turn with the thread service tier").await;

    // The status while idle: the thread, the account (no sign-in with this provider), and the
    // rate limits from Codex's rolling update (the read needs an OpenAI sign-in).
    let sections = r.handle.control.status().await.unwrap();
    let titles: Vec<&str> = sections.iter().map(|s| s.title.as_str()).collect();
    assert_eq!(titles, ["Codex thread", "Account", "Rate limits"]);
    assert_eq!(row(&sections, "Codex thread", "Model"), Some("gpt-5.6-sol"));
    assert_eq!(row(&sections, "Codex thread", "Service tier"), Some("Fast"));
    assert_eq!(
        row(&sections, "Account", "Sign-in"),
        Some("not required by the configured model provider")
    );
    let primary = row(&sections, "Rate limits", "Primary limit").unwrap();
    assert!(
        primary.starts_with("13% used \u{b7} 5h window \u{b7} "),
        "{primary}"
    );
    assert_eq!(row(&sections, "Rate limits", "Credits"), Some("17.5"));

    // Fast mode off: the next turn clears the tier.
    assert_eq!(
        r.handle
            .control
            .apply_modes(&ThreadModes::default())
            .await
            .unwrap(),
        SettingsApplied::Live
    );
    let events = turn(&mut r, "ROLE=SAY_T2# turn clearing the tier").await;
    assert!(events.iter().any(|e| matches!(
        e,
        AdapterEvent::ModesReported { fast_state: Some(s), .. } if s == "default"
    )));
    // A model without a fast mode refuses it.
    r.handle
        .control
        .apply_settings(&ThreadSettings {
            model: Some("gpt-5.2".into()),
            ..settings
        })
        .await
        .unwrap();
    let error = r
        .handle
        .control
        .apply_modes(&ThreadModes {
            plan: false,
            fast: true,
        })
        .await
        .unwrap_err();
    assert!(error.detail().contains("gpt-5.2"), "{error}");
    shutdown_cleanly(r).await;
}

// ----- goals ------------------------------------------------------------------------------------

const OBJECTIVE: &str = "ROLE=GOAL# Make hello.txt say GOAL_OK.";

/// Sends a `/goal` command and returns the events of its own turn (the answer's notice).
async fn goal(r: &mut Replay, text: &str) -> Vec<AdapterEvent> {
    turn(r, text).await
}

fn only_notice(events: &[AdapterEvent]) -> (NoticeLevel, String) {
    let found = notices(events);
    assert_eq!(found.len(), 1, "{events:#?}");
    (found[0].0, found[0].1.clone())
}

/// Drives `goal.jsonl` from its start until the third continuation has started (the one the
/// tests interrupt): the forms of `/goal`, the goal's own turn and two continuations.
async fn goal_until_the_third_continuation(r: &mut Replay) {
    let events = goal(r, "/goal").await;
    assert_eq!(
        only_notice(&events),
        (NoticeLevel::Info, "No goal is currently set.".into())
    );
    assert!(anchors(&events).is_empty(), "a command is not a Codex turn");
    let events = goal(r, "/goal clear").await;
    assert_eq!(
        only_notice(&events).1,
        "This thread does not currently have a goal."
    );
    let error = r
        .handle
        .control
        .send(TurnInput::text("/goal pause"))
        .await
        .unwrap_err();
    assert_eq!(
        error.detail(),
        "thread/goal/set: cannot update goal for thread 01a0e942-f3f9-7043-9953-b09f33b519e9: no goal exists"
    );
    // A malformed `/goal` is refused without asking Codex.
    let error = r
        .handle
        .control
        .send(TurnInput::text("/goal edit"))
        .await
        .unwrap_err();
    assert!(
        error
            .detail()
            .starts_with("Usage: /goal [<objective>|clear|edit|pause|resume]"),
        "{error}"
    );

    // Setting the goal: its own turn, then the continuations Codex starts by itself.
    let from = r.events.len();
    r.handle
        .control
        .send(TurnInput::text(format!("/goal {OBJECTIVE}")))
        .await
        .unwrap();
    let command_end = r.until(is_turn_completed).await;
    assert_eq!(completed_status(&command_end), TurnStatus::Completed);
    assert_eq!(
        only_notice(&r.events[from..]),
        (NoticeLevel::Info, format!("Goal active: {OBJECTIVE}"))
    );
    for continuation in [
        "01a0e942-f6f0-71f1-9cab-baff6a1bcf88",
        "01a0e943-073c-75d0-b9ad-b92442596d52",
    ] {
        let from = r.events.len();
        assert_eq!(
            completed_status(&r.until(is_turn_completed).await),
            TurnStatus::Completed
        );
        let events = &r.events[from..];
        assert!(
            matches!(events[0], AdapterEvent::TurnStarted),
            "{events:#?}"
        );
        assert_eq!(anchors(events), [json!({ "turnId": continuation })]);
        // The accounting update of every turn keeps the status: no notice.
        assert!(notices(events).is_empty(), "{events:#?}");
    }
    r.until(|e| matches!(e, AdapterEvent::TurnStarted)).await;
}

#[tokio::test]
async fn goals_are_commands_and_codexs_continuations_are_its_own_turns() {
    let mut r = support::start_with(
        "goal.jsonl",
        StartMode::New,
        ThreadSettings::default(),
        StartOptions::default(),
        &[],
    )
    .await;
    goal_until_the_third_continuation(&mut r).await;
    // The third continuation is interrupted: as in Codex's TUI, the active goal is paused with
    // it (written before the interrupt), and the turn says so before it ends.
    let from = r.events.len();
    r.handle.control.interrupt().await.unwrap();
    assert_eq!(
        completed_status(&r.until(is_turn_completed).await),
        TurnStatus::Interrupted
    );
    assert_eq!(
        notices(&r.events[from..]),
        [(
            NoticeLevel::Info,
            format!("Goal paused: {OBJECTIVE}"),
            "goalUpdated".to_owned()
        )]
    );
    let events = goal(&mut r, "/goal").await;
    assert_eq!(
        only_notice(&events).1,
        format!("Goal paused: {OBJECTIVE} (1,620 tokens, 19s used)")
    );
    // Resumed, Codex continues; the model completes the goal within that turn.
    let events = goal(&mut r, "/goal resume").await;
    assert_eq!(only_notice(&events).1, format!("Goal active: {OBJECTIVE}"));
    let from = r.events.len();
    r.until(is_turn_completed).await;
    let events = &r.events[from..];
    assert_eq!(
        anchors(events),
        [json!({ "turnId": "01a0e943-5a31-7172-b680-10c4c4ff7952" })]
    );
    assert_eq!(
        notices(events),
        [(
            NoticeLevel::Info,
            format!("Goal complete: {OBJECTIVE}"),
            "goalUpdated".to_owned()
        )]
    );

    let sections = r.handle.control.status().await.unwrap();
    assert_eq!(row(&sections, "Goal", "Status"), Some("complete"));
    assert_eq!(row(&sections, "Goal", "Objective"), Some(OBJECTIVE));
    assert_eq!(
        row(&sections, "Rate limits", "Not available"),
        Some("codex account authentication required to read rate limits")
    );
    shutdown_cleanly(r).await;
}

/// Codex refuses to pause the goal of an interrupted turn: the turn is still interrupted, and
/// it says that the goal goes on.
#[tokio::test]
async fn a_goal_codex_does_not_pause_with_an_interrupt_is_told() {
    let mut entries = support::script("goal.jsonl");
    let answer = entries
        .iter()
        .position(|e| e["respondsTo"] == "7p")
        .expect("the pause's answer");
    entries[answer] = json!({"s": {"id": 0, "error": {"code": -32600, "message": "cannot pause the goal"}},
        "respondsTo": "7p"});
    // Without the pause, Codex reports no paused goal; the replay ends with the interrupted turn.
    assert_eq!(
        entries[answer + 1]["s"]["params"]["goal"]["status"],
        "paused"
    );
    entries.remove(answer + 1);
    let end = entries
        .iter()
        .position(|e| {
            e["s"]["method"] == "turn/completed"
                && e["s"]["params"]["turn"]["status"] == "interrupted"
        })
        .expect("the interrupted turn");
    entries.truncate(end + 1);
    let mut r = support::try_start_entries(
        entries,
        StartMode::New,
        ThreadSettings::default(),
        StartOptions::default(),
        &[],
        support::policy(),
    )
    .await
    .unwrap_or_else(|(e, _)| panic!("establish failed: {e}"));
    goal_until_the_third_continuation(&mut r).await;
    let from = r.events.len();
    r.handle.control.interrupt().await.unwrap();
    assert_eq!(
        completed_status(&r.until(is_turn_completed).await),
        TurnStatus::Interrupted
    );
    let told = notices(&r.events[from..]);
    assert_eq!(told.len(), 1, "{told:?}");
    assert_eq!(
        (told[0].0, told[0].2.as_str()),
        (NoticeLevel::Warning, "goalNotPaused")
    );
    assert!(
        told[0]
            .1
            .ends_with("thread/goal/set: cannot pause the goal"),
        "{told:?}"
    );
    shutdown_cleanly(r).await;
}

// ----- features ---------------------------------------------------------------------------------

#[test]
fn features_follow_what_codex_offers() {
    // The user's catalog (DeepSeek through Codex): no service tier, so no fast mode.
    let user = aas_adapter_codex::testing::features_for_models(json!([
        {"id":"deepseek-flash","displayName":"DeepSeek-Flash","isDefault":true,"hidden":false,
         "serviceTiers":[],"defaultServiceTier":null,"additionalSpeedTiers":[]}
    ]))
    .unwrap();
    assert!(user.fast_mode_models.is_empty());
    assert!(user.fork_at_turn && user.fork_while_held && user.rename && user.status);
    assert!(!user.side_question && !user.move_to_background && !user.project_trust);
    let plan = user.plan_mode.expect("plan mode");
    assert_eq!(
        plan.implement_prompt.as_deref(),
        Some("Implement the plan.")
    );
    assert!(
        plan.new_thread_preamble
            .as_deref()
            .is_some_and(|p| p.starts_with("A previous agent produced the plan below"))
    );
    // codex-cli 0.148.0's bundled catalog: the GPT models list one tier, "Fast".
    let bundled = aas_adapter_codex::testing::features_for_models(json!([
        {"id":"gpt-5.6-sol","isDefault":true,"hidden":false,
         "serviceTiers":[{"id":"priority","name":"Fast","description":"1.5x speed, increased usage"}]},
        {"id":"gpt-5.2","isDefault":false,"hidden":false,"serviceTiers":[]}
    ]))
    .unwrap();
    assert_eq!(bundled.fast_mode_models, ["gpt-5.6-sol"]);
}

#[test]
fn imported_turns_carry_their_ids_as_anchors() {
    let (history, anchors) = aas_adapter_codex::testing::anchored_history_from_thread_read(
        json!({"thread": {"id": "t", "preview": "p", "turns": [
            {"id": "a", "status": "completed", "items": [
                {"type":"userMessage","id":"item-1","content":[{"type":"text","text":"plan it"}]},
                {"type":"plan","id":"a-plan","text":"# Plan\n"},
                {"type":"agentMessage","id":"item-2","text":"done"}]},
            {"id": "b", "status": "completed", "items": []}
        ]}}),
        std::path::Path::new(r"C:\WORKSPACE"),
        &support::policy(),
    )
    .unwrap();
    assert_eq!(
        anchors,
        [Some(json!({"turnId": "a"})), Some(json!({"turnId": "b"}))]
    );
    assert!(
        matches!(&history.turns[0].items[1].body, ItemBody::ProposedPlan { text } if text == "# Plan\n")
    );
}

#[tokio::test]
async fn a_goal_is_paused_or_cleared_while_its_continuation_runs() {
    const OBJECTIVE3: &str = "ROLE=GOAL# objective for running tests";
    let mut r = support::start_with(
        "goal_steer.jsonl",
        StartMode::New,
        ThreadSettings::default(),
        StartOptions::default(),
        &[],
    )
    .await;
    goal(&mut r, &format!("/goal {OBJECTIVE3}")).await;
    // The first continuation runs: `/goal pause` goes to Codex as a steer would (its answer is
    // a notice of the running turn), and the continuation finishes.
    r.until(|e| matches!(e, AdapterEvent::TurnStarted)).await;
    let from = r.events.len();
    r.handle
        .control
        .steer(TurnInput::text("/goal pause"))
        .await
        .unwrap();
    // Commands that run as turns of their own are refused while a turn runs; nothing is sent.
    let error = r
        .handle
        .control
        .steer(TurnInput::text("/review check it"))
        .await
        .unwrap_err();
    assert_eq!(
        error.detail(),
        "/review runs as a turn of its own: send it when no turn runs"
    );
    assert_eq!(
        completed_status(&r.until(is_turn_completed).await),
        TurnStatus::Completed
    );
    assert_eq!(
        notices(&r.events[from..]),
        [(
            NoticeLevel::Info,
            format!("Goal paused: {OBJECTIVE3}"),
            "goal".to_owned()
        )]
    );
    // Resumed: continuations until the goal is cleared during the third one.
    goal(&mut r, "/goal resume").await;
    for _ in 0..2 {
        r.until(is_turn_completed).await;
    }
    r.until(|e| matches!(e, AdapterEvent::TurnStarted)).await;
    let from = r.events.len();
    r.handle
        .control
        .steer(TurnInput::text("/goal clear"))
        .await
        .unwrap();
    r.until(is_turn_completed).await;
    assert_eq!(
        notices(&r.events[from..]),
        [(
            NoticeLevel::Info,
            "Goal cleared".to_owned(),
            "goal".to_owned()
        )]
    );
    let events = goal(&mut r, "/goal clear").await;
    assert_eq!(
        only_notice(&events).1,
        "This thread does not currently have a goal."
    );
    shutdown_cleanly(r).await;
}
