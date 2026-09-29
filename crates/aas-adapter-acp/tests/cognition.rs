//! Replays of Devin CLI 3000.11.3 (`devin acp`) recordings for Cognition's extensions besides
//! background work (docs/adapters/acp.md §17): turn anchors at steps and forks at a step
//! (revert), the session's name, Devin's own statistics, its own mode commands, and the
//! commands this adapter does not offer. The fixtures were converted from the rec2 recordings
//! by a script (paths, session ids and MCP logs replaced or removed; see acp.md §14).

mod common;

use std::path::PathBuf;
use std::time::Duration;

use aas_adapter_acp::testing::{
    AdapterOptions, FakeLink, Mode, Steps, launch_with_io, launch_with_steps, source_steps_with_io,
};
use aas_harness::protocol::{
    InteractionRequest, InteractionResolution, ItemBody, NoticeLevel, StatusRow, StatusSection,
    TurnStatus,
};
use aas_harness::{
    AdapterError, AdapterEvent, AdapterPolicy, ForkPoint, StopReason, ThreadSettings, TurnInput,
};
use common::*;
use pretty_assertions::assert_eq;
use serde_json::{Value, json};

fn cwd() -> PathBuf {
    PathBuf::from(r"C:\work\proj")
}

fn policy() -> AdapterPolicy {
    AdapterPolicy {
        stop_grace: Duration::from_secs(2),
        handshake_timeout: Duration::from_secs(10),
        ..AdapterPolicy::default()
    }
}

/// The recorded steps' ids (rv-main: ONE, TWO, THREE, FOUR).
const STEP_ONE: &str = "79ab27de-6ccb-4342-8c48-715e78f79616";
const STEP_TWO: &str = "2561db7b-3419-43e5-9597-d316f2994eee";
const STEP_THREE: &str = "25e2fe14-9436-4d52-b981-3871fc0cfb89";
const STEP_FOUR: &str = "f3fc0c12-b820-4f67-885d-6c67439fd82d";

fn anchor(step: &str, nodes: Option<(i64, i64)>) -> Value {
    match nodes {
        Some((revert, fork)) => json!({"stepIds": [step], "revertTargetNodeId": revert,
                                       "forkTargetNodeId": fork}),
        None => json!({"stepIds": [step]}),
    }
}

fn is_replaced(ev: &AdapterEvent) -> bool {
    matches!(ev, AdapterEvent::TurnAnchorReplaced { .. })
}

fn row(label: &str, value: &str) -> StatusRow {
    StatusRow {
        label: label.into(),
        value: value.into(),
    }
}

/// Recording `revert`: four prompts with the revert extension. Each turn reports its step as
/// its anchor before `TurnCompleted`; `listSteps`, asked right after the prompt answered,
/// settles the node ids (`TurnAnchorReplaced`); the next prompt's `stepsUpdated`, which agrees,
/// changes nothing. The statistics of the last turn are the status; a rename is echoed.
#[tokio::test]
async fn devin_turns_are_anchored_at_their_steps() {
    let link = FakeLink::default();
    let (reader, writer, agent) = spawn_agent(
        load_fixture("devin_revert.jsonl"),
        link.clone(),
        End::OnClientEof,
    );
    let steps = Steps::default();
    let launched = launch_with_steps(
        (reader, writer, link),
        cwd(),
        Mode::New,
        ThreadSettings::default(),
        AdapterOptions::default(),
        policy(),
        steps.clone(),
    )
    .await
    .expect("launch");
    assert_eq!(
        launched.handle.native_session_id.as_deref(),
        Some("rv-main")
    );
    let control = launched.handle.control;
    let mut rx = launched.handle.events;
    let mut f = Folded::default();
    // A new session has no steps before its first turn.
    assert_eq!(steps.of("rv-main"), Some((Vec::new(), true)));

    let prompts = [
        "Reply with exactly: ONE",
        "Create a file named step2.txt in the current directory containing exactly the word two. Then reply with exactly: TWO",
        "Reply with exactly: THREE",
    ];
    for (i, prompt) in prompts.iter().enumerate() {
        control.send(TurnInput::text(*prompt)).await.unwrap();
        loop {
            let ev = pump_until(&mut rx, &mut f, |ev| {
                matches!(
                    ev,
                    AdapterEvent::InteractionRequested { .. } | AdapterEvent::TurnCompleted { .. }
                )
            })
            .await;
            match ev {
                AdapterEvent::InteractionRequested { request_id, .. } => control
                    .respond(
                        &request_id,
                        &InteractionResolution::Approval {
                            option_id: "allow_session".into(),
                            feedback: None,
                        },
                    )
                    .await
                    .unwrap(),
                _ => break,
            }
        }
        // The steps are listed after the prompt answered: the anchor settles.
        pump_until(&mut rx, &mut f, is_replaced).await;
        assert_eq!(f.turns.len(), i + 1);
    }
    let anchors: Vec<&AdapterEvent> = f
        .events
        .iter()
        .filter(|e| {
            matches!(
                e,
                AdapterEvent::TurnAnchor { .. } | AdapterEvent::TurnAnchorReplaced { .. }
            )
        })
        .collect();
    assert_eq!(
        anchors,
        vec![
            &AdapterEvent::TurnAnchor {
                anchor: anchor(STEP_ONE, None)
            },
            &AdapterEvent::TurnAnchorReplaced {
                previous: anchor(STEP_ONE, None),
                anchor: anchor(STEP_ONE, Some((19, 23)))
            },
            &AdapterEvent::TurnAnchor {
                anchor: anchor(STEP_TWO, None)
            },
            &AdapterEvent::TurnAnchorReplaced {
                previous: anchor(STEP_TWO, None),
                anchor: anchor(STEP_TWO, Some((23, 29)))
            },
            &AdapterEvent::TurnAnchor {
                anchor: anchor(STEP_THREE, None)
            },
            &AdapterEvent::TurnAnchorReplaced {
                previous: anchor(STEP_THREE, None),
                anchor: anchor(STEP_THREE, Some((29, 32)))
            },
        ]
    );
    // Each turn's anchor comes before its end.
    let first_anchor = f.position(|e| matches!(e, AdapterEvent::TurnAnchor { .. }));
    let first_end = f.position(is_turn_completed);
    assert!(first_anchor < first_end);

    // The last turn's statistics, in Devin's groups and words.
    let status = control.status().await.unwrap();
    assert_eq!(
        status,
        vec![
            StatusSection {
                title: "Response Statistics (last turn)".into(),
                rows: vec![
                    row("Agent messages", "1 message"),
                    row("Model", "SWE-2 High")
                ]
            },
            StatusSection {
                title: "Token Usage (last turn)".into(),
                rows: vec![
                    row("Input tokens", "3046 tokens"),
                    row("Output tokens", "22 tokens"),
                    row("Cached input tokens", "9216 tokens")
                ]
            },
        ]
    );

    // The user's title reaches the session; Devin echoes it.
    control.rename("rec2 revert original").await.unwrap();
    pump_until(
        &mut rx,
        &mut f,
        |e| matches!(e, AdapterEvent::SessionTitle { title } if title == "rec2 revert original"),
    )
    .await;

    // The fourth prompt: its start lists the earlier steps as settled (no replacement).
    control
        .send(TurnInput::text("Reply with exactly: FOUR"))
        .await
        .unwrap();
    pump_until(&mut rx, &mut f, is_turn_completed).await;
    let replaced = pump_until(&mut rx, &mut f, is_replaced).await;
    assert_eq!(
        replaced,
        AdapterEvent::TurnAnchorReplaced {
            previous: anchor(STEP_FOUR, None),
            anchor: anchor(STEP_FOUR, Some((32, 35)))
        }
    );

    // What the adapter's sessions share for forks: every step, listed after its turn.
    let (listed, complete) = steps.of("rv-main").unwrap();
    assert!(complete);
    assert_eq!(
        listed,
        vec![
            (STEP_ONE.to_owned(), 19, 23),
            (STEP_TWO.to_owned(), 23, 29),
            (STEP_THREE.to_owned(), 29, 32),
            (STEP_FOUR.to_owned(), 32, 35),
        ]
    );
    // A fork of the whole session, at a turn, or before it, without reading the source.
    assert_eq!(steps.fork_target("rv-main", None), Ok(Some(35)));
    let provisional = ForkPoint {
        anchor: anchor(STEP_TWO, None),
        before: false,
        previous: None,
    };
    assert_eq!(
        steps.fork_target("rv-main", Some(&provisional)),
        Ok(Some(29))
    );
    let before = ForkPoint {
        before: true,
        ..provisional
    };
    assert_eq!(steps.fork_target("rv-main", Some(&before)), Ok(Some(23)));
    assert_eq!(steps.fork_target("unknown", Some(&before)), Ok(None));

    control.shutdown(StopReason::User).await;
    drain_to_exit(&mut rx, &mut f).await;
    let client = agent.finish().await;
    assert_eq!(f.turns.len(), 4);
    assert!(f.turns.iter().all(|t| t.0 == TurnStatus::Completed));
    // `listSteps` follows each prompt; the rename names the session.
    let methods: Vec<&str> = client
        .iter()
        .filter_map(|m| m.get("method").and_then(Value::as_str))
        .collect();
    assert_eq!(
        methods,
        vec![
            "initialize",
            "session/new",
            "session/prompt",
            "_cognition.ai/revert/listSteps",
            "session/prompt",
            "_cognition.ai/revert/listSteps",
            "session/prompt",
            "_cognition.ai/revert/listSteps",
            "_cognition.ai/session/rename",
            "session/prompt",
            "_cognition.ai/revert/listSteps",
        ]
    );
    let rename = client
        .iter()
        .find(|m| m["method"] == "_cognition.ai/session/rename")
        .unwrap();
    assert_eq!(
        rename["params"],
        json!({"sessionId": "rv-main", "title": "rec2 revert original"})
    );
    assert!(f.notices.is_empty(), "{:?}", f.notices);
}

/// A fork process: `forkFromStep` of rv-main at node 29 (the second turn included), then the
/// branch is loaded (its replay is history already) and its steps listed. Spliced from the
/// recordings of the same sessions (see the fixture's notes in acp.md §14).
#[tokio::test]
async fn a_fork_at_a_turn_branches_with_fork_from_step() {
    let link = FakeLink::default();
    let (reader, writer, agent) = spawn_agent(
        load_fixture("devin_revert_fork.jsonl"),
        link.clone(),
        End::OnClientEof,
    );
    let steps = Steps::default();
    let launched = launch_with_steps(
        (reader, writer, link),
        cwd(),
        Mode::ForkFromStep {
            source: "rv-main".into(),
            node: 29,
        },
        ThreadSettings::default(),
        AdapterOptions::default(),
        policy(),
        steps.clone(),
    )
    .await
    .expect("launch");
    assert_eq!(
        launched.handle.native_session_id.as_deref(),
        Some("rv-fork-2")
    );
    // The branch keeps the source's steps and nodes: the copied turns' anchors stay valid.
    assert_eq!(
        steps.of("rv-fork-2"),
        Some((
            vec![(STEP_ONE.to_owned(), 19, 23), (STEP_TWO.to_owned(), 23, 29)],
            true
        ))
    );
    let control = launched.handle.control;
    let mut rx = launched.handle.events;
    let mut f = Folded::default();
    control.shutdown(StopReason::User).await;
    drain_to_exit(&mut rx, &mut f).await;
    // The replay made no items and no turns.
    assert!(f.items.is_empty() && f.turns.is_empty(), "{:?}", f.items);
    let client = agent.finish().await;
    let fork = client
        .iter()
        .find(|m| m["method"] == "_cognition.ai/revert/forkFromStep")
        .unwrap();
    assert_eq!(
        fork["params"],
        json!({"sessionId": "rv-main", "targetNodeId": 29})
    );
    let load = client
        .iter()
        .find(|m| m["method"] == "session/load")
        .unwrap();
    assert_eq!(load["params"]["sessionId"], "rv-fork-2");
}

/// Recording `revert-load`: a history read with the revert extension. Each turn's anchor is its
/// step, found by the replayed user chunk's `cognition.ai/clientMessageId`, with the node ids
/// `listSteps` gives for the loaded session.
#[tokio::test]
async fn history_turns_carry_their_step_anchors() {
    let link = FakeLink::default();
    let (reader, writer, agent) = spawn_agent(
        load_fixture("devin_revert_history.jsonl"),
        link.clone(),
        End::OnClientEof,
    );
    let launched = launch_with_io(
        reader,
        writer,
        link,
        cwd(),
        Mode::History("rv-fork-3".into()),
        ThreadSettings::default(),
        AdapterOptions::default(),
        policy(),
    )
    .await
    .expect("launch");
    let history = launched.history.expect("history");
    let users: Vec<&str> = history
        .turns
        .iter()
        .filter_map(|t| match &t.items[0].body {
            ItemBody::UserMessage { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        users,
        vec![
            "Reply with exactly: ONE",
            "Create a file named step2.txt in the current directory containing exactly the word two. Then reply with exactly: TWO",
            "Reply with exactly: THREE"
        ]
    );
    assert_eq!(
        launched.history_anchors,
        vec![
            Some(anchor(STEP_ONE, Some((19, 23)))),
            Some(anchor(STEP_TWO, Some((23, 29)))),
            Some(anchor(STEP_THREE, Some((29, 32)))),
        ]
    );
    let control = launched.handle.control;
    control.shutdown(StopReason::Shutdown).await;
    let client = agent.finish().await;
    assert!(
        client
            .iter()
            .any(|m| m["method"] == "_cognition.ai/revert/listSteps"
                && m["params"]["sessionId"] == "rv-fork-3")
    );
}

/// Recording `revert2-other`: a turn whose nodes were never listed is read by loading the
/// source; while another process holds it, Devin replays it and then refuses (-32015), and the
/// fork fails with Devin's reason.
#[tokio::test]
async fn reading_a_held_session_for_its_steps_fails_with_devins_reason() {
    let link = FakeLink::default();
    let (reader, writer, agent) = spawn_agent(
        load_fixture("devin_revert_held.jsonl"),
        link,
        End::OnClientEof,
    );
    let err = source_steps_with_io(reader, writer, &cwd(), "rv-held")
        .await
        .unwrap_err();
    match &err {
        AdapterError::Harness(message) => {
            assert!(
                message.contains("could not be read to list it")
                    && message.contains("is already open in another process"),
                "{message}"
            );
        }
        other => panic!("{other:?}"),
    }
    agent.finish().await;
}

/// Recording `modes`: Devin's own `/ask`, `/plan`, `/code`, `/smart`, `/bypass` (no model call)
/// and leaving plan mode through its approval change the mode, reported as the permission mode
/// (the engine reflects it into the thread); `/ask <question>` does not. Devin's account
/// commands are not offered.
#[tokio::test]
async fn devins_own_mode_commands_are_reported_as_the_permission_mode() {
    let link = FakeLink::default();
    let (reader, writer, agent) = spawn_agent(
        load_fixture("devin_modes.jsonl"),
        link.clone(),
        End::OnClientEof,
    );
    let launched = launch_with_io(
        reader,
        writer,
        link,
        cwd(),
        Mode::New,
        ThreadSettings::default(),
        AdapterOptions::default(),
        policy(),
    )
    .await
    .expect("launch");
    let control = launched.handle.control;
    let mut rx = launched.handle.events;
    let mut f = Folded::default();
    let mut modes_after = Vec::new();
    for prompt in [
        "/ask",
        "/plan",
        "/code",
        "/smart",
        "/bypass",
        "/code",
        "/ask What is 2+2? Reply with just the number.",
        "/plan Plan how to create a file hello.txt containing hi. Keep the plan to two short steps and do not explore the repository.",
    ] {
        control.send(TurnInput::text(prompt)).await.unwrap();
        loop {
            let ev = pump_until(&mut rx, &mut f, |ev| {
                matches!(
                    ev,
                    AdapterEvent::InteractionRequested { .. } | AdapterEvent::TurnCompleted { .. }
                )
            })
            .await;
            match ev {
                AdapterEvent::InteractionRequested {
                    request_id,
                    request: InteractionRequest::Approval { options, .. },
                    ..
                } => {
                    // Leaving plan mode: implement with accept-edits; the command after it: once.
                    let option = if options.iter().any(|o| o.id == "plan_accept_edits") {
                        "plan_accept_edits"
                    } else {
                        "allow_once"
                    };
                    control
                        .respond(
                            &request_id,
                            &InteractionResolution::Approval {
                                option_id: option.into(),
                                feedback: None,
                            },
                        )
                        .await
                        .unwrap();
                }
                AdapterEvent::TurnCompleted { .. } => break,
                other => panic!("{other:?}"),
            }
        }
        modes_after.push(f.infos.last().and_then(|i| i.1.clone()));
    }
    control.shutdown(StopReason::User).await;
    drain_to_exit(&mut rx, &mut f).await;
    agent.finish().await;
    let mode = |m: &str| Some(m.to_owned());
    assert_eq!(
        modes_after,
        vec![
            mode("ask"),
            mode("plan"),
            mode("accept-edits"),
            mode("smart"),
            mode("bypass"),
            mode("accept-edits"),
            mode("accept-edits"),
            // `/plan <task>` entered plan mode; its approval left it.
            mode("accept-edits"),
        ]
    );
    let reported: Vec<Option<String>> = f.infos.iter().map(|i| i.1.clone()).collect();
    assert_eq!(
        reported,
        vec![
            mode("accept-edits"),
            mode("ask"),
            mode("plan"),
            mode("accept-edits"),
            mode("smart"),
            mode("bypass"),
            mode("accept-edits"),
            mode("plan"),
            mode("accept-edits"),
        ]
    );
    // Devin reports its thought level explicitly (a config option): the effort.
    assert!(f.infos.iter().all(|i| i.2.as_deref() == Some("high")));
    // Devin's own `/plan` stays a command; its account commands are not offered.
    assert!(f.commands.iter().all(
        |c| c.contains(&"plan".to_owned()) && !c.iter().any(|n| n == "login" || n == "logout")
    ));
    // Without the revert extension (not declared in this recording) no turn is anchored.
    assert!(
        !f.events
            .iter()
            .any(|e| matches!(e, AdapterEvent::TurnAnchor { .. }))
    );
    assert_eq!(f.turns.len(), 8);
}

/// Recording `caps0`: the user's title is given to the session before and after a prompt;
/// Devin echoes it, and around the prompt sends its own title and the user's again. The
/// prompt's statistics are the status. An empty title is refused without asking Devin.
#[tokio::test]
async fn renames_reach_the_session_and_devin_echoes_them() {
    let link = FakeLink::default();
    let (reader, writer, agent) = spawn_agent(
        load_fixture("devin_rename.jsonl"),
        link.clone(),
        End::OnClientEof,
    );
    let launched = launch_with_io(
        reader,
        writer,
        link,
        cwd(),
        Mode::New,
        ThreadSettings::default(),
        AdapterOptions::default(),
        policy(),
    )
    .await
    .expect("launch");
    let control = launched.handle.control;
    let mut rx = launched.handle.events;
    let mut f = Folded::default();
    assert!(matches!(
        control.rename("   ").await,
        Err(AdapterError::Other(_))
    ));
    control.rename("rec2 renamed before prompt").await.unwrap();
    control
        .send(TurnInput::text("Reply with exactly: ONE"))
        .await
        .unwrap();
    pump_until(&mut rx, &mut f, is_turn_completed).await;
    assert_eq!(
        control.status().await.unwrap()[1],
        StatusSection {
            title: "Token Usage (last turn)".into(),
            rows: vec![
                row("Input tokens", "11909 tokens"),
                row("Output tokens", "27 tokens"),
                row("Cached input tokens", "0 tokens")
            ]
        }
    );
    // Devin's titles after the turn (they may be read before the prompt's answer is handled),
    // then the second rename.
    let devins_title =
        |e: &AdapterEvent| matches!(e, AdapterEvent::SessionTitle { title } if title == "ONE");
    if !f.events.iter().any(devins_title) {
        pump_until(&mut rx, &mut f, devins_title).await;
    }
    control.rename("rec2 renamed after prompt").await.unwrap();
    control.shutdown(StopReason::User).await;
    drain_to_exit(&mut rx, &mut f).await;
    agent.finish().await;
    let titles: Vec<&str> = f
        .events
        .iter()
        .filter_map(|e| match e {
            AdapterEvent::SessionTitle { title } => Some(title.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        titles,
        vec![
            "rec2 renamed before prompt",
            "Reply with exactly: ONE",
            "rec2 renamed before prompt",
            "ONE",
            "rec2 renamed before prompt",
            "rec2 renamed after prompt",
        ]
    );
    // The agent did not confirm the revert extension here: no step anchors.
    assert!(
        !f.events
            .iter()
            .any(|e| matches!(e, AdapterEvent::TurnAnchor { .. }))
    );
}

fn handshake(caps: Value) -> Vec<Step> {
    vec![
        Step::request("initialize", 1),
        Step::Send(json!({"jsonrpc": "2.0", "id": 1, "result": {
            "protocolVersion": 1, "agentCapabilities": caps, "authMethods": [],
            "agentInfo": {"name": "scripted", "version": "1.0.0"}
        }})),
        Step::request("session/new", 2),
        Step::Send(json!({"jsonrpc": "2.0", "id": 2, "result": {"sessionId": "s1"}})),
    ]
}

/// Hand-written: `_cognition.ai/billingInformation` was never recorded; its fields are the
/// binary's serde names (`BillingInformationNotification { title, body }`). It becomes a notice
/// and part of the status; one of another session is ignored.
#[tokio::test]
async fn billing_information_becomes_a_notice_and_part_of_the_status() {
    let billing = |session: &str| {
        Step::Send(
            json!({"jsonrpc": "2.0", "method": "_cognition.ai/billingInformation",
            "params": {"sessionId": session, "title": "Billing",
                       "body": "The agent auto-continued past the per-turn billing threshold."}}),
        )
    };
    let mut steps =
        handshake(json!({"loadSession": true, "_meta": {"cognition.ai/sessionRename": true}}));
    steps.extend([
        Step::request("session/prompt", 3),
        billing("other"),
        billing("s1"),
        Step::Send(json!({"jsonrpc": "2.0", "id": 3, "result": {"stopReason": "end_turn"}})),
    ]);
    let link = FakeLink::default();
    let (reader, writer, agent) = spawn_agent(steps, link.clone(), End::OnClientEof);
    let launched = launch_with_io(
        reader,
        writer,
        link,
        cwd(),
        Mode::New,
        ThreadSettings::default(),
        AdapterOptions::default(),
        policy(),
    )
    .await
    .expect("launch");
    let control = launched.handle.control;
    let mut rx = launched.handle.events;
    let mut f = Folded::default();
    control.send(TurnInput::text("go")).await.unwrap();
    pump_until(&mut rx, &mut f, is_turn_completed).await;
    assert_eq!(
        f.notices,
        vec![(
            NoticeLevel::Info,
            "Billing\nThe agent auto-continued past the per-turn billing threshold.".to_owned(),
            Some("billingInformation".to_owned())
        )]
    );
    assert_eq!(
        control.status().await.unwrap(),
        vec![StatusSection {
            title: "Billing information".into(),
            rows: vec![row(
                "Billing",
                "The agent auto-continued past the per-turn billing threshold."
            )]
        }]
    );
    control.shutdown(StopReason::User).await;
    drain_to_exit(&mut rx, &mut f).await;
    agent.finish().await;
}

/// Hand-written: a plain ACP agent confirms none of Cognition's extensions. Nothing is renamed,
/// no steps are listed, and its own `login` command is its own.
#[tokio::test]
async fn a_plain_acp_agent_gets_none_of_cognitions_extensions() {
    let mut steps = handshake(json!({"loadSession": true}));
    steps.extend([
        Step::Send(
            json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "s1",
            "update": {"sessionUpdate": "available_commands_update", "availableCommands": [
                {"name": "login", "description": "Sign in"}]}}}),
        ),
        Step::request("session/prompt", 3),
        Step::Send(
            json!({"jsonrpc": "2.0", "id": 3, "result": {"stopReason": "end_turn",
            "_meta": {"cognition.ai/userMessageId": "m1"}}}),
        ),
    ]);
    let link = FakeLink::default();
    let (reader, writer, agent) = spawn_agent(steps, link.clone(), End::OnClientEof);
    let launched = launch_with_io(
        reader,
        writer,
        link,
        cwd(),
        Mode::New,
        ThreadSettings::default(),
        AdapterOptions::default(),
        policy(),
    )
    .await
    .expect("launch");
    let control = launched.handle.control;
    let mut rx = launched.handle.events;
    let mut f = Folded::default();
    assert_eq!(
        control.rename("t").await,
        Err(AdapterError::Unsupported("rename"))
    );
    control.send(TurnInput::text("go")).await.unwrap();
    pump_until(&mut rx, &mut f, is_turn_completed).await;
    assert_eq!(control.status().await.unwrap(), Vec::new());
    control.shutdown(StopReason::User).await;
    drain_to_exit(&mut rx, &mut f).await;
    let client = agent.finish().await;
    assert_eq!(f.commands, vec![vec!["login".to_owned()]]);
    assert!(
        !f.events
            .iter()
            .any(|e| matches!(e, AdapterEvent::TurnAnchor { .. }))
    );
    assert!(
        !client
            .iter()
            .any(|m| m["method"] == "_cognition.ai/revert/listSteps")
    );
}
