//! One example of every message. Used to generate the golden fixtures under
//! `fixtures/protocol/` (shared with the Android client) and by tests across the workspace.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use crate::events::{Event, EventEnvelope};
use crate::http::*;
use crate::ids::*;
use crate::methods::*;
use crate::notifications::*;
use crate::rpc::{ErrorKind, RequestId, RpcError, RpcMessage};
use crate::types::*;

const T0: Millis = 1_790_000_000_000;

pub fn project_id() -> ProjectId {
    ProjectId::from("prj_01K6A0000000000000000PRJ01")
}
pub fn thread_id() -> ThreadId {
    ThreadId::from("thr_01K6A0000000000000000THR01")
}
pub fn turn_id() -> TurnId {
    TurnId::from("trn_01K6A0000000000000000TRN01")
}
pub fn item_id(n: u32) -> ItemId {
    ItemId::from(format!("itm_01K6A00000000000000000IT{n:02}"))
}
pub fn interaction_id(n: u32) -> InteractionId {
    InteractionId::from(format!("int_01K6A00000000000000000IN{n:02}"))
}
pub fn device_id() -> DeviceId {
    DeviceId::from("dev_01K6A0000000000000000DEV01")
}
pub fn blob_id() -> BlobId {
    BlobId::from_sha256_hex("9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08")
}
pub fn queued_id() -> QueuedInputId {
    QueuedInputId::from("que_01K6A0000000000000000QUE01")
}
pub fn operation_id() -> OperationId {
    OperationId::from("op_01K6A00000000000000000OP01")
}
pub fn background_task_id(n: u32) -> BackgroundTaskId {
    BackgroundTaskId::from(format!("bgt_01K6A00000000000000000BG{n:02}"))
}

pub fn harness() -> Harness {
    Harness {
        id: "claude".into(),
        kind: HarnessKind::Claude,
        display_name: "Claude Code".into(),
        available: true,
        unavailable_reason: None,
        version: Some("2.1.283".into()),
        executable: Some(r"C:\nvm4w\nodejs\claude.cmd".into()),
        capabilities: HarnessCapabilities {
            interrupt: true,
            steer: false,
            approvals: true,
            questions: true,
            resume: true,
            fork: true,
            images: true,
            model_switch_live: true,
            native_sessions: true,
            background_tasks: true,
            background_stop: true,
        },
        models: vec![
            Model {
                id: "opus".into(),
                display_name: "Opus".into(),
                description: None,
                is_default: true,
                effort_levels: None,
            },
            Model {
                id: "sonnet".into(),
                display_name: "Sonnet".into(),
                description: Some("Fast".into()),
                is_default: false,
                effort_levels: Some(vec!["low".into(), "medium".into(), "high".into()]),
            },
        ],
        default_model: Some("opus".into()),
        effort_levels: vec![
            EffortLevel {
                id: "low".into(),
                label: "Low".into(),
            },
            EffortLevel {
                id: "medium".into(),
                label: "Medium".into(),
            },
            EffortLevel {
                id: "high".into(),
                label: "High".into(),
            },
        ],
        permission_modes: vec![
            PermissionMode {
                id: "default".into(),
                label: "Ask".into(),
                description: Some("Ask before edits and commands".into()),
                is_default: true,
            },
            PermissionMode {
                id: "acceptEdits".into(),
                label: "Accept edits".into(),
                description: None,
                is_default: false,
            },
        ],
        default_permission_mode: Some("default".into()),
        features: HarnessFeatures {
            fork_at_turn: true,
            fork_while_held: true,
            rename: true,
            side_question: true,
            move_to_background: true,
            status: true,
            project_trust: false,
            plan_mode: Some(PlanModeFeature::default()),
            fast_mode_models: vec!["opus".into()],
        },
    }
}

/// The plan mode of a harness that continues from a proposed plan with its own texts (Codex).
pub fn implementing_plan_mode() -> PlanModeFeature {
    PlanModeFeature {
        implement_prompt: Some("Implement the plan.".into()),
        new_thread_preamble: Some("A previous agent produced the plan below to accomplish the user's task. Implement the plan in a fresh context. Treat the plan as the source of user intent, re-read files as needed, and carry the work through implementation and verification.".into()),
    }
}

pub fn project() -> Project {
    Project {
        id: project_id(),
        name: "agent-app-server".into(),
        path: r"C:\Users\me\Documents\agent-app-server".into(),
        created_at: T0,
        updated_at: T0 + 1_000,
        archived: false,
        defaults: ProjectDefaults {
            harness_id: Some("claude".into()),
            model: Some("opus".into()),
            effort: None,
            permission_mode: Some("default".into()),
        },
        git: GitInfo {
            is_repo: true,
            branch: Some("main".into()),
            root: Some(r"C:\Users\me\Documents\agent-app-server".into()),
        },
        harness_trust: [("pi".to_owned(), true)].into_iter().collect(),
    }
}

pub fn turn() -> Turn {
    Turn {
        id: turn_id(),
        thread_id: thread_id(),
        index: 0,
        status: TurnStatus::Completed,
        started_at: T0 + 2_000,
        completed_at: Some(T0 + 60_000),
        model: Some("opus".into()),
        error: None,
        usage: Some(usage()),
        diff: Some(DiffSummary {
            files: 2,
            insertions: 40,
            deletions: 3,
        }),
        trigger: None,
        forkable: true,
    }
}

/// A turn the agent started by itself because a background task ended.
pub fn triggered_turn() -> Turn {
    Turn {
        id: TurnId::from("trn_01K6A0000000000000000TRN02"),
        index: 1,
        started_at: T0 + 90_000,
        completed_at: Some(T0 + 95_000),
        diff: None,
        trigger: Some(TurnTrigger::BackgroundTask),
        forkable: false,
        ..turn()
    }
}

pub fn usage() -> Usage {
    Usage {
        input_tokens: 12_000,
        output_tokens: 900,
        cached_input_tokens: 8_000,
        reasoning_tokens: 300,
        cost_usd: Some(0.12),
        context: Some(ContextUsage {
            used_tokens: 84_000,
            window_tokens: 200_000,
        }),
    }
}

pub fn thread() -> Thread {
    Thread {
        id: thread_id(),
        project_id: project_id(),
        harness_id: "claude".into(),
        title: "Fix the flaky reconnect test".into(),
        cwd: r"C:\Users\me\Documents\agent-app-server".into(),
        workspace: Workspace::Local,
        settings: ThreadSettings {
            model: Some("opus".into()),
            effort: Some("high".into()),
            permission_mode: Some("default".into()),
        },
        status: ThreadStatus::Running,
        pending_interactions: 1,
        queued_inputs: 1,
        queue_paused: false,
        last_turn: Some(TurnSummary {
            id: turn_id(),
            index: 0,
            status: TurnStatus::Running,
            started_at: T0 + 2_000,
            completed_at: None,
        }),
        last_error: Some(ThreadError {
            message: "agent exited with code 1".into(),
            kind: "agentExited".into(),
            at: T0 + 1_500,
        }),
        native_session_id: Some("7c2d1c1e-5b0e-4a51-9b1e-2f0c3a1b9d10".into()),
        forked_from: None,
        usage: usage(),
        diff_available: true,
        created_at: T0,
        updated_at: T0 + 2_000,
        last_activity_at: T0 + 2_000,
        archived: false,
        pinned: true,
        background: ThreadBackground {
            running: 1,
            last_ended: Some(BackgroundTaskEnded {
                task_id: background_task_id(2),
                title: "npm run build".into(),
                kind: BackgroundTaskKind::Shell,
                status: BackgroundTaskStatus::Completed,
                ended_at: T0 + 88_000,
            }),
        },
        modes: ThreadModes {
            plan: false,
            fast: true,
        },
        fast_mode_state: Some("on".into()),
        head: 42,
    }
}

pub fn worktree_thread() -> Thread {
    Thread {
        id: ThreadId::from("thr_01K6A0000000000000000THR02"),
        workspace: Workspace::Worktree {
            path: r"C:\Users\me\AppData\Local\agent-app-server\worktrees\prj_x\thr_y".into(),
            branch: "aas/thr_y".into(),
            base_ref: "main".into(),
        },
        status: ThreadStatus::Idle,
        pending_interactions: 0,
        queued_inputs: 0,
        last_error: None,
        forked_from: Some(ForkOrigin {
            thread_id: thread_id(),
            turn_id: Some(turn_id()),
        }),
        pinned: false,
        background: ThreadBackground::default(),
        modes: ThreadModes::default(),
        fast_mode_state: None,
        ..thread()
    }
}

fn item(n: u32, status: ItemStatus, body: ItemBody) -> Item {
    Item {
        id: item_id(n),
        thread_id: thread_id(),
        turn_id: turn_id(),
        status,
        started_at: T0 + 2_000 + n as i64,
        completed_at: (status != ItemStatus::InProgress).then_some(T0 + 3_000 + n as i64),
        background_task_id: None,
        backgroundable: false,
        body,
    }
}

/// One item of every kind.
pub fn items() -> Vec<Item> {
    vec![
        item(
            1,
            ItemStatus::Completed,
            ItemBody::UserMessage {
                text: "Fix the flaky test in @crates/aas-server/tests/reconnect.rs".into(),
                attachments: vec![Attachment::Image {
                    blob_id: blob_id(),
                    mime: "image/png".into(),
                }],
                mentions: vec![Mention {
                    path: "crates/aas-server/tests/reconnect.rs".into(),
                }],
                delivery: UserMessageDelivery::Normal,
            },
        ),
        item(
            2,
            ItemStatus::Completed,
            ItemBody::Reasoning {
                text: "The test races the heartbeat.".into(),
            },
        ),
        item(
            3,
            ItemStatus::Completed,
            ItemBody::CommandExecution {
                command: "cargo test -p aas-server reconnect".into(),
                cwd: Some(r"C:\Users\me\Documents\agent-app-server".into()),
                output: "running 3 tests\ntest ok\n".into(),
                output_truncated: false,
                output_blob_id: None,
                exit_code: Some(0),
                duration_ms: Some(5_400),
            },
        ),
        item(
            4,
            ItemStatus::Completed,
            ItemBody::FileChange {
                changes: vec![FileChange {
                    path: "crates/aas-server/tests/reconnect.rs".into(),
                    kind: FileChangeKind::Update,
                    move_path: None,
                    diff: Some("@@ -1,3 +1,3 @@\n-let t = 1;\n+let t = 2;\n".into()),
                    added: Some(1),
                    removed: Some(1),
                }],
            },
        ),
        item(
            5,
            ItemStatus::Completed,
            ItemBody::ToolCall {
                category: ToolCategory::Mcp,
                name: "search_docs".into(),
                title: "docs: search_docs".into(),
                server: Some("docs".into()),
                input: Some(json!({"query": "tokio watch"})),
                output: Some("3 results".into()),
                output_truncated: false,
                output_blob_id: None,
            },
        ),
        item(
            6,
            ItemStatus::Completed,
            ItemBody::Plan {
                entries: vec![
                    PlanEntry {
                        text: "Reproduce".into(),
                        status: PlanEntryStatus::Completed,
                    },
                    PlanEntry {
                        text: "Fix".into(),
                        status: PlanEntryStatus::InProgress,
                    },
                    PlanEntry {
                        text: "Verify".into(),
                        status: PlanEntryStatus::Pending,
                    },
                ],
            },
        ),
        item(
            7,
            ItemStatus::InProgress,
            ItemBody::AgentMessage {
                text: "I found the race: the heartbeat".into(),
            },
        ),
        item(
            8,
            ItemStatus::Completed,
            ItemBody::Notice {
                level: NoticeLevel::Warning,
                message: "Context compacted".into(),
                code: Some("compacted".into()),
            },
        ),
        backgrounded_item(),
        item(
            10,
            ItemStatus::Completed,
            ItemBody::ProposedPlan {
                text: "1. Reproduce the race\n2. Order the ack before the heartbeat\n3. Run the reconnect tests\n".into(),
            },
        ),
        backgroundable_item(),
    ]
}

/// A running command the harness says can be moved to the background now.
pub fn backgroundable_item() -> Item {
    Item {
        backgroundable: true,
        ..item(
            11,
            ItemStatus::InProgress,
            ItemBody::CommandExecution {
                command: "npm run dev".into(),
                cwd: Some(r"C:\Users\me\Documents\agent-app-server".into()),
                output: "ready on http://localhost:5173\n".into(),
                output_truncated: false,
                output_blob_id: None,
                exit_code: None,
                duration_ms: None,
            },
        )
    }
}

/// The item that launched background task 1 (a sub-agent that goes on after the turn).
pub fn backgrounded_item() -> Item {
    Item {
        background_task_id: Some(background_task_id(1)),
        ..item(
            9,
            ItemStatus::Backgrounded,
            ItemBody::ToolCall {
                category: ToolCategory::Subagent,
                name: "Agent".into(),
                title: "Review the reconnect logic".into(),
                server: None,
                input: Some(
                    json!({"description": "Review the reconnect logic", "run_in_background": true}),
                ),
                output: Some("Async agent launched".into()),
                output_truncated: false,
                output_blob_id: None,
            },
        )
    }
}

/// A running background agent with progress.
pub fn background_task() -> BackgroundTask {
    BackgroundTask {
        id: background_task_id(1),
        thread_id: thread_id(),
        native_id: "a546c1f2e9d04b7a8".into(),
        kind: BackgroundTaskKind::Agent,
        title: "Review the reconnect logic".into(),
        status: BackgroundTaskStatus::Running,
        ambient: false,
        runs: 1,
        turn_id: Some(turn_id()),
        origin_item_id: Some(item_id(9)),
        parent_task_id: None,
        started_at: T0 + 3_100,
        ended_at: None,
        end_reason: None,
        progress: Some(BackgroundProgress {
            last_tool_name: Some("Read".into()),
            tool_uses: Some(7),
            tokens: Some(18_400),
            duration_ms: Some(42_000),
            summary: None,
            workflow: Vec::new(),
        }),
        result: None,
        usage: None,
        stoppable: true,
        stop_requested_at: None,
        stop_unconfirmed_at: None,
        next_run_at: None,
    }
}

/// A workflow whose agents report their state, stopped on request.
pub fn stopped_workflow() -> BackgroundTask {
    BackgroundTask {
        id: background_task_id(3),
        native_id: "w7k2m9q1".into(),
        kind: BackgroundTaskKind::Workflow,
        title: "review-and-fix".into(),
        status: BackgroundTaskStatus::Stopped,
        origin_item_id: None,
        parent_task_id: Some(background_task_id(1)),
        ended_at: Some(T0 + 60_000),
        end_reason: Some(BackgroundEndReason::Harness),
        progress: Some(BackgroundProgress {
            summary: Some("Review, then fix what the review finds".into()),
            workflow: vec![
                WorkflowAgent {
                    label: "review".into(),
                    phase: Some("analyze".into()),
                    state: WorkflowAgentState::Done,
                    agent_type: Some("general-purpose".into()),
                    model: Some("haiku".into()),
                    tokens: Some(15_751),
                },
                WorkflowAgent {
                    label: "fix".into(),
                    phase: Some("apply".into()),
                    state: WorkflowAgentState::Progress,
                    agent_type: None,
                    model: None,
                    tokens: None,
                },
            ],
            ..Default::default()
        }),
        result: Some(BackgroundResult {
            summary: Some("Stopped by request".into()),
            ..Default::default()
        }),
        usage: Some(BackgroundUsage {
            total_tokens: Some(31_504),
            tool_uses: Some(12),
            duration_ms: Some(56_900),
            cost_usd: None,
        }),
        ..background_task()
    }
}

/// A shell task that finished, with its exit code and output.
pub fn finished_shell() -> BackgroundTask {
    BackgroundTask {
        id: background_task_id(2),
        native_id: "bkomsmz3d".into(),
        kind: BackgroundTaskKind::Shell,
        title: "npm run build".into(),
        status: BackgroundTaskStatus::Completed,
        origin_item_id: None,
        ended_at: Some(T0 + 88_000),
        end_reason: Some(BackgroundEndReason::Harness),
        progress: None,
        result: Some(BackgroundResult {
            summary: None,
            exit_code: Some(0),
            output: Some("built in 41.2s\n".into()),
            output_truncated: false,
            output_blob_id: None,
        }),
        ..background_task()
    }
}

/// An approval a background agent asks for (it survives the end of the turn).
pub fn background_approval() -> Interaction {
    Interaction {
        id: interaction_id(3),
        turn_id: None,
        item_id: None,
        background_task_id: Some(background_task_id(1)),
        created_at: T0 + 10_840,
        ..approval()
    }
}

pub fn approval() -> Interaction {
    Interaction {
        id: interaction_id(1),
        thread_id: thread_id(),
        turn_id: Some(turn_id()),
        item_id: Some(item_id(3)),
        background_task_id: None,
        status: InteractionStatus::Pending,
        created_at: T0 + 2_500,
        resolved_at: None,
        resolved_by: None,
        request: InteractionRequest::Approval {
            title: "Run command?".into(),
            detail: Some("The agent wants to run a shell command.".into()),
            subject: Subject::Command {
                command: "cargo test -p aas-server".into(),
                cwd: Some(r"C:\Users\me\Documents\agent-app-server".into()),
            },
            options: vec![
                ApprovalOption {
                    id: "allow".into(),
                    label: "Allow".into(),
                    kind: ApprovalOptionKind::AllowOnce,
                },
                ApprovalOption {
                    id: "allow_session".into(),
                    label: "Allow for this session".into(),
                    kind: ApprovalOptionKind::AllowForSession,
                },
                ApprovalOption {
                    id: "deny".into(),
                    label: "Deny".into(),
                    kind: ApprovalOptionKind::Deny,
                },
            ],
        },
        resolution: None,
        expire_reason: None,
    }
}

pub fn resolved_approval() -> Interaction {
    Interaction {
        status: InteractionStatus::Resolved,
        resolved_at: Some(T0 + 2_700),
        resolved_by: Some(device_id().into_string()),
        resolution: Some(InteractionResolution::Approval {
            option_id: "allow".into(),
            feedback: None,
        }),
        ..approval()
    }
}

pub fn question() -> Interaction {
    Interaction {
        id: interaction_id(2),
        thread_id: thread_id(),
        turn_id: Some(turn_id()),
        item_id: None,
        background_task_id: None,
        status: InteractionStatus::Pending,
        created_at: T0 + 2_600,
        resolved_at: None,
        resolved_by: None,
        request: InteractionRequest::Question {
            title: "Which database?".into(),
            questions: vec![Question {
                id: "q1".into(),
                header: Some("DB".into()),
                prompt: "Which database should the migration target?".into(),
                choices: vec![
                    QuestionChoice {
                        id: "sqlite".into(),
                        label: "SQLite".into(),
                        description: None,
                    },
                    QuestionChoice {
                        id: "pg".into(),
                        label: "PostgreSQL".into(),
                        description: Some("Production".into()),
                    },
                ],
                multi_select: false,
                allow_free_text: true,
                placeholder: Some("Other…".into()),
            }],
        },
        resolution: None,
        expire_reason: None,
    }
}

pub fn expired_question() -> Interaction {
    Interaction {
        status: InteractionStatus::Expired,
        resolved_at: Some(T0 + 9_000),
        resolved_by: Some("system".into()),
        expire_reason: Some(ExpireReason::ProcessExited),
        ..question()
    }
}

pub fn queued() -> QueuedInput {
    QueuedInput {
        id: queued_id(),
        thread_id: thread_id(),
        created_at: T0 + 3_000,
        preview: "Also update the docs".into(),
        input: vec![InputPart::Text {
            text: "Also update the docs".into(),
        }],
    }
}

pub fn operation() -> Operation {
    Operation {
        id: operation_id(),
        kind: OperationKind::GitClone,
        status: OperationStatus::Running,
        project_id: None,
        message: Some("Cloning https://github.com/example/new-app.git".into()),
        progress: Some("Receiving objects:  42% (420/1000), 1.20 MiB | 2.40 MiB/s".into()),
        started_at: T0,
        finished_at: None,
    }
}

pub fn cancelled_operation() -> Operation {
    Operation {
        status: OperationStatus::Cancelled,
        message: Some("Cancelled".into()),
        progress: None,
        finished_at: Some(T0 + 5_000),
        ..operation()
    }
}

pub fn device() -> Device {
    Device {
        id: device_id(),
        name: "Pixel".into(),
        platform: Some("android".into()),
        created_at: T0,
        last_seen_at: Some(T0 + 10_000),
        current: true,
    }
}

pub fn commands() -> Vec<Command> {
    vec![
        Command {
            name: "model".into(),
            description: Some("Change the model".into()),
            source: CommandSource::App,
            argument_hint: None,
            action: CommandAction::Picker {
                picker: PickerKind::Model,
            },
        },
        Command {
            name: "fork".into(),
            description: Some("Fork this thread".into()),
            source: CommandSource::App,
            argument_hint: None,
            action: CommandAction::Method {
                method: "thread/fork".into(),
                params: None,
            },
        },
        Command {
            name: "compact".into(),
            description: Some("Compact the conversation".into()),
            source: CommandSource::Harness,
            argument_hint: Some("[instructions]".into()),
            action: CommandAction::InsertText {
                text: "/compact ".into(),
            },
        },
    ]
}

fn crid(n: u32) -> String {
    format!("0199a5f0-0000-7000-8000-{n:012}")
}

/// Every request with an example result, in method-table order.
pub fn request_examples() -> Vec<(ClientRequest, Value)> {
    let v = |x: &dyn erased::Ser| x.to_value();
    vec![
        (
            ClientRequest::Initialize(InitializeParams {
                protocol_version: 1,
                client: ClientInfo {
                    name: "aas-android".into(),
                    version: "0.1.0".into(),
                    platform: "android".into(),
                },
                last_known_epoch: Some("01K6A0000000000000000EPOCH".into()),
            }),
            v(&InitializeResult {
                protocol_version: 1,
                server: ServerInfo {
                    name: "home-pc".into(),
                    version: "0.1.0".into(),
                    hostname: "DESKTOP-1".into(),
                    epoch: "01K6A0000000000000000EPOCH".into(),
                },
                device: DeviceInfo {
                    id: device_id(),
                    name: "Pixel".into(),
                },
                epoch_changed: false,
                policy: ClientPolicy {
                    heartbeat_interval_ms: 15_000,
                    client_timeout_ms: 45_000,
                    max_client_frame_bytes: 1_048_576,
                    max_blob_bytes: 26_214_400,
                },
            }),
        ),
        (
            ClientRequest::Subscribe(SubscribeParams {
                subscriptions: vec![
                    Subscription {
                        stream: "workspace".into(),
                        after: 311,
                    },
                    Subscription {
                        stream: crate::thread_stream(&thread_id()),
                        after: 1842,
                    },
                ],
            }),
            v(&SubscribeResult {
                subscriptions: vec![
                    SubscriptionStatus {
                        stream: "workspace".into(),
                        head: 400,
                        status: SubscriptionState::Ok,
                    },
                    SubscriptionStatus {
                        stream: crate::thread_stream(&thread_id()),
                        head: 1850,
                        status: SubscriptionState::Ok,
                    },
                ],
            }),
        ),
        (
            ClientRequest::Unsubscribe(UnsubscribeParams {
                streams: vec![crate::thread_stream(&thread_id())],
            }),
            v(&Empty {}),
        ),
        (
            ClientRequest::WorkspaceSnapshot(Empty {}),
            v(&WorkspaceSnapshotResult {
                harnesses: vec![harness()],
                projects: vec![project()],
                threads: vec![thread()],
                pending_interactions: vec![approval()],
                operations: vec![operation()],
                head: 400,
            }),
        ),
        (
            ClientRequest::ServerStatus(Empty {}),
            v(&ServerStatusResult {
                uptime_ms: 3_600_000,
                running_processes: 2,
                running_turns: 1,
                draining: false,
                prevent_sleep_while_running: true,
                running_background_tasks: 1,
            }),
        ),
        (
            ClientRequest::DeviceList(Empty {}),
            v(&DeviceListResult {
                devices: vec![device()],
            }),
        ),
        (
            ClientRequest::DeviceRevoke(DeviceRevokeParams {
                client_request_id: crid(1),
                device_id: device_id(),
            }),
            v(&Empty {}),
        ),
        (
            ClientRequest::HarnessList(Empty {}),
            v(&HarnessListResult {
                harnesses: vec![harness()],
            }),
        ),
        (
            ClientRequest::HarnessRefresh(HarnessRefreshParams {
                harness_id: Some("claude".into()),
            }),
            v(&HarnessListResult {
                harnesses: vec![harness()],
            }),
        ),
        (
            ClientRequest::ProjectList(ProjectListParams {
                include_archived: false,
            }),
            v(&ProjectListResult {
                projects: vec![project()],
            }),
        ),
        (
            ClientRequest::ProjectGet(ProjectGetParams {
                project_id: project_id(),
            }),
            v(&ProjectResult { project: project() }),
        ),
        (
            ClientRequest::ProjectCreate(ProjectCreateParams {
                client_request_id: crid(2),
                parent_path: r"C:\Users\me\Documents".into(),
                name: "new-app".into(),
                init: ProjectInit::GitClone {
                    url: "https://github.com/example/new-app.git".into(),
                },
            }),
            v(&ProjectCreateResult {
                project: None,
                operation: Some(operation()),
            }),
        ),
        (
            ClientRequest::ProjectOpen(ProjectOpenParams {
                client_request_id: crid(3),
                path: r"C:\Users\me\Documents\agent-app-server".into(),
                name: None,
            }),
            v(&ProjectResult { project: project() }),
        ),
        (
            ClientRequest::ProjectUpdate(ProjectUpdateParams {
                client_request_id: crid(4),
                project_id: project_id(),
                name: Some("aas".into()),
                defaults: Some(ProjectDefaults {
                    harness_id: Some("codex".into()),
                    ..Default::default()
                }),
                harness_trust: Some([("pi".to_owned(), true)].into_iter().collect()),
            }),
            v(&ProjectResult { project: project() }),
        ),
        (
            ClientRequest::ProjectArchive(ProjectArchiveParams {
                client_request_id: crid(5),
                project_id: project_id(),
                archived: true,
            }),
            v(&ProjectResult {
                project: Project {
                    archived: true,
                    ..project()
                },
            }),
        ),
        (
            ClientRequest::ProjectRemove(ProjectRemoveParams {
                client_request_id: crid(6),
                project_id: project_id(),
            }),
            v(&Empty {}),
        ),
        (
            ClientRequest::FsRoots(Empty {}),
            v(&FsRootsResult {
                roots: vec![FsRoot {
                    path: r"C:\Users\me\Documents".into(),
                    name: "Documents".into(),
                }],
            }),
        ),
        (
            ClientRequest::FsList(FsListParams {
                path: r"C:\Users\me\Documents".into(),
                include_files: false,
            }),
            v(&FsListResult {
                path: r"C:\Users\me\Documents".into(),
                entries: vec![FsEntry {
                    name: "agent-app-server".into(),
                    path: r"C:\Users\me\Documents\agent-app-server".into(),
                    is_dir: true,
                    is_git_repo: Some(true),
                    size: None,
                    modified_at: Some(T0),
                }],
            }),
        ),
        (
            ClientRequest::FsMkdir(FsMkdirParams {
                client_request_id: crid(7),
                path: r"C:\Users\me\Documents\new-app".into(),
            }),
            v(&FsMkdirResult {
                path: r"C:\Users\me\Documents\new-app".into(),
            }),
        ),
        (
            ClientRequest::FsSearch(FsSearchParams {
                project_id: Some(project_id()),
                thread_id: None,
                query: "reconn".into(),
                limit: Some(20),
            }),
            v(&FsSearchResult {
                results: vec![SearchResult {
                    path: "crates/aas-server/tests/reconnect.rs".into(),
                    is_dir: false,
                }],
                ranking: "heuristic:H1".into(),
            }),
        ),
        (
            ClientRequest::ThreadList(ThreadListParams {
                project_id: Some(project_id()),
                include_archived: false,
                limit: Some(50),
                before: Some(ThreadCursor {
                    last_activity_at: T0 + 9_000,
                    id: thread_id(),
                }),
            }),
            v(&ThreadListResult {
                threads: vec![thread(), worktree_thread()],
                has_more: false,
            }),
        ),
        (
            ClientRequest::ThreadGet(ThreadGetParams {
                thread_id: thread_id(),
            }),
            v(&ThreadResult { thread: thread() }),
        ),
        (
            ClientRequest::ThreadCreate(ThreadCreateParams {
                client_request_id: crid(8),
                project_id: project_id(),
                harness_id: "claude".into(),
                settings: Some(ThreadSettings {
                    model: Some("opus".into()),
                    effort: Some("high".into()),
                    permission_mode: None,
                }),
                workspace: Some(WorkspaceSpec::Worktree {
                    base_ref: Some("main".into()),
                    branch: None,
                }),
                title: None,
                input: Some(vec![
                    InputPart::Text {
                        text: "Fix the flaky test".into(),
                    },
                    InputPart::Mention {
                        path: "crates/aas-server/tests/reconnect.rs".into(),
                    },
                    InputPart::Image { blob_id: blob_id() },
                ]),
            }),
            v(&ThreadCreateResult {
                thread: worktree_thread(),
                turn_id: Some(turn_id()),
                disposition: Some(Disposition::Started),
            }),
        ),
        (
            ClientRequest::ThreadRead(ThreadReadParams {
                thread_id: thread_id(),
                before_turn_index: None,
                limit_turns: Some(20),
            }),
            v(&ThreadReadResult {
                thread: thread(),
                turns: vec![turn(), triggered_turn()],
                items: items(),
                interactions: vec![approval(), question(), background_approval()],
                queued: vec![queued()],
                background_tasks: vec![background_task(), finished_shell(), stopped_workflow()],
                head: 1850,
                has_more_before: false,
            }),
        ),
        (
            ClientRequest::ThreadUpdate(ThreadUpdateParams {
                client_request_id: crid(9),
                thread_id: thread_id(),
                title: Some("Reconnect race".into()),
                settings: Some(ThreadSettings {
                    model: Some("sonnet".into()),
                    ..Default::default()
                }),
                pinned: Some(true),
                modes: Some(ThreadModesUpdate {
                    plan: Some(true),
                    fast: None,
                }),
            }),
            v(&ThreadUpdateResult {
                thread: thread(),
                settings_outcome: Some(SettingsOutcome::AppliesNextTurn),
                native_rename: Some(NativeRename {
                    status: NativeRenameStatus::Applied,
                    message: None,
                }),
            }),
        ),
        (
            ClientRequest::ThreadArchive(ThreadArchiveParams {
                client_request_id: crid(10),
                thread_id: thread_id(),
                archived: true,
                remove_worktree: true,
                force: false,
            }),
            v(&ThreadResult {
                thread: Thread {
                    archived: true,
                    ..thread()
                },
            }),
        ),
        (
            ClientRequest::ThreadFork(ThreadForkParams {
                client_request_id: crid(11),
                thread_id: thread_id(),
                at_turn_id: Some(turn_id()),
                before: true,
            }),
            v(&ThreadResult {
                thread: worktree_thread(),
            }),
        ),
        (
            ClientRequest::ThreadStop(ThreadStopParams {
                client_request_id: crid(12),
                thread_id: thread_id(),
            }),
            v(&ThreadResult {
                thread: Thread {
                    status: ThreadStatus::Idle,
                    ..thread()
                },
            }),
        ),
        (
            ClientRequest::ThreadDiff(ThreadDiffParams {
                thread_id: thread_id(),
                scope: DiffScope::Turn { turn_id: turn_id() },
            }),
            v(&ThreadDiffResult {
                summary: DiffSummary {
                    files: 1,
                    insertions: 1,
                    deletions: 1,
                },
                files: vec![DiffFile {
                    path: "crates/aas-server/tests/reconnect.rs".into(),
                    kind: FileChangeKind::Update,
                    added: 1,
                    removed: 1,
                    binary: false,
                }],
                patch: Some("diff --git a/x b/x\n".into()),
                patch_blob_id: None,
            }),
        ),
        (
            ClientRequest::TurnStart(TurnStartParams {
                client_request_id: crid(13),
                thread_id: thread_id(),
                input: vec![InputPart::Text {
                    text: "Also update the docs".into(),
                }],
                delivery: Delivery::Queue,
            }),
            v(&TurnStartResult {
                disposition: Disposition::Queued,
                turn_id: None,
                queued_id: Some(queued_id()),
            }),
        ),
        (
            ClientRequest::TurnInterrupt(TurnInterruptParams {
                client_request_id: crid(14),
                thread_id: thread_id(),
            }),
            v(&TurnInterruptResult { interrupted: true }),
        ),
        (
            ClientRequest::QueueRemove(QueueRemoveParams {
                client_request_id: crid(15),
                thread_id: thread_id(),
                queued_id: queued_id(),
            }),
            v(&QueueRemoveResult { removed: true }),
        ),
        (
            ClientRequest::QueueResume(QueueResumeParams {
                client_request_id: crid(18),
                thread_id: thread_id(),
            }),
            v(&QueueResumeResult {
                turn_id: Some(turn_id()),
            }),
        ),
        (
            ClientRequest::QueueUpdate(QueueUpdateParams {
                client_request_id: crid(19),
                thread_id: thread_id(),
                queued_id: queued_id(),
                input: vec![
                    InputPart::Text {
                        text: "Also update the docs and the changelog".into(),
                    },
                    InputPart::Mention {
                        path: "CHANGELOG.md".into(),
                    },
                ],
            }),
            v(&QueueUpdateResult { updated: true }),
        ),
        (
            ClientRequest::QueueSteer(QueueSteerParams {
                client_request_id: crid(20),
                thread_id: thread_id(),
                queued_id: queued_id(),
            }),
            v(&QueueSteerResult {
                disposition: Some(Disposition::Steered),
                turn_id: Some(turn_id()),
            }),
        ),
        (
            ClientRequest::InteractionRespond(InteractionRespondParams {
                client_request_id: crid(16),
                interaction_id: interaction_id(1),
                resolution: InteractionResolution::Approval {
                    option_id: "allow".into(),
                    feedback: None,
                },
            }),
            v(&InteractionRespondResult {
                interaction: resolved_approval(),
                already_resolved: false,
            }),
        ),
        (
            ClientRequest::InteractionList(InteractionListParams {
                status: Some(InteractionStatus::Pending),
            }),
            v(&InteractionListResult {
                interactions: vec![approval(), question()],
            }),
        ),
        (
            ClientRequest::CommandList(CommandListParams {
                thread_id: Some(thread_id()),
                project_id: None,
                harness_id: None,
            }),
            v(&CommandListResult {
                commands: commands(),
            }),
        ),
        (
            ClientRequest::NativeList(NativeListParams {
                project_id: project_id(),
                harness_id: "codex".into(),
            }),
            v(&NativeListResult {
                sessions: vec![NativeSession {
                    native_session_id: "0199a5f0-1111-7000-8000-000000000001".into(),
                    title: Some("Refactor supervisor".into()),
                    updated_at: Some(T0 - 86_400_000),
                    cwd: Some(r"C:\Users\me\Documents\agent-app-server".into()),
                    imported_thread_id: None,
                }],
            }),
        ),
        (
            ClientRequest::NativeImport(NativeImportParams {
                client_request_id: crid(17),
                project_id: project_id(),
                harness_id: "codex".into(),
                native_session_id: "0199a5f0-1111-7000-8000-000000000001".into(),
            }),
            v(&ThreadResult { thread: thread() }),
        ),
        (
            ClientRequest::OperationList(Empty {}),
            v(&OperationListResult {
                operations: vec![operation()],
            }),
        ),
        (
            ClientRequest::OperationCancel(OperationCancelParams {
                client_request_id: crid(21),
                operation_id: operation_id(),
            }),
            v(&OperationResult {
                operation: cancelled_operation(),
            }),
        ),
        (
            ClientRequest::BackgroundTaskStop(BackgroundTaskStopParams {
                client_request_id: crid(22),
                thread_id: thread_id(),
                task_id: background_task_id(1),
            }),
            v(&BackgroundTaskResult {
                task: BackgroundTask {
                    stop_requested_at: Some(T0 + 17_970),
                    ..background_task()
                },
            }),
        ),
        (
            ClientRequest::ThreadHarnessStatus(ThreadHarnessStatusParams {
                thread_id: thread_id(),
            }),
            v(&ThreadHarnessStatusResult {
                sections: vec![
                    StatusSection {
                        title: "Session".into(),
                        rows: vec![
                            StatusRow {
                                label: "Version".into(),
                                value: "2.1.284".into(),
                            },
                            StatusRow {
                                label: "Login method".into(),
                                value: "Claude Max account".into(),
                            },
                        ],
                    },
                    StatusSection {
                        title: "Usage limits".into(),
                        rows: vec![StatusRow {
                            label: "5-hour".into(),
                            value: "13% used, resets 18:00".into(),
                        }],
                    },
                ],
                live: true,
            }),
        ),
        (
            ClientRequest::ThreadSideQuestion(ThreadSideQuestionParams {
                thread_id: thread_id(),
                question: "Which file holds the heartbeat timer?".into(),
            }),
            v(&ThreadSideQuestionResult {
                answer: Some("crates/aas-server/src/conn.rs (`InboundDeadline`).".into()),
                synthetic: false,
            }),
        ),
        (
            ClientRequest::ItemMoveToBackground(ItemMoveToBackgroundParams {
                client_request_id: crid(23),
                thread_id: thread_id(),
                item_id: item_id(11),
            }),
            v(&Empty {}),
        ),
    ]
}

/// Every request, in method-table order.
pub fn all_requests() -> Vec<ClientRequest> {
    request_examples().into_iter().map(|(r, _)| r).collect()
}

/// One event of every type.
pub fn events() -> Vec<EventEnvelope> {
    let items = items();
    let mut seq = 100;
    let mut env = |event: Event| {
        seq += 1;
        EventEnvelope {
            seq,
            seq_from: None,
            ts: T0 + seq as i64,
            event,
        }
    };
    let mut out = vec![
        env(Event::ProjectUpserted { project: project() }),
        env(Event::ProjectRemoved {
            project_id: project_id(),
        }),
        env(Event::ThreadUpserted { thread: thread() }),
        env(Event::ThreadRemoved {
            thread_id: thread_id(),
        }),
        env(Event::InteractionPending {
            interaction: approval(),
        }),
        env(Event::InteractionClosed {
            interaction_id: interaction_id(1),
            thread_id: thread_id(),
            status: InteractionStatus::Resolved,
        }),
        env(Event::HarnessUpdated { harness: harness() }),
        env(Event::OperationUpdated {
            operation: operation(),
        }),
        env(Event::ThreadUpdated { thread: thread() }),
        env(Event::TurnStarted {
            turn: Turn {
                status: TurnStatus::Running,
                completed_at: None,
                ..turn()
            },
        }),
        env(Event::TurnCompleted { turn: turn() }),
        env(Event::TurnDiffUpdated {
            turn_id: turn_id(),
            diff: DiffSummary {
                files: 2,
                insertions: 40,
                deletions: 3,
            },
        }),
        env(Event::TurnUsageUpdated {
            turn_id: turn_id(),
            usage: usage(),
        }),
        env(Event::ItemStarted {
            item: items[6].clone(),
        }),
        env(Event::ItemUpdated {
            item: items[5].clone(),
        }),
        env(Event::ItemCompleted {
            item: items[2].clone(),
        }),
        env(Event::InteractionRequested {
            interaction: approval(),
        }),
        env(Event::InteractionResolved {
            interaction: resolved_approval(),
        }),
        env(Event::InteractionExpired {
            interaction: expired_question(),
        }),
        env(Event::QueueUpdated {
            queued: vec![queued()],
        }),
        env(Event::CommandsChanged {}),
        env(Event::BackgroundTaskUpdated {
            task: background_task(),
        }),
        env(Event::NativeSessionChanged {
            previous_native_session_id: "7c2d1c1e-5b0e-4a51-9b1e-2f0c3a1b9d10".into(),
            native_session_id: "0b6f3a52-91c4-4f1e-8d2a-5e7c9a0d4b11".into(),
        }),
        env(Event::ComposerInsert {
            text: "Summarize the changes so far".into(),
        }),
        env(Event::Native {
            harness_id: "codex".into(),
            payload: json!({"method": "model/rerouted"}),
        }),
    ];
    // A coalesced delta.
    out.push(EventEnvelope {
        seq: 130,
        seq_from: Some(122),
        ts: T0 + 130,
        event: Event::ItemDelta {
            item_id: item_id(7),
            field: DeltaField::Text,
            text: " races the ack".into(),
        },
    });
    out
}

pub fn notifications() -> Vec<ServerNotification> {
    let mut heads = BTreeMap::new();
    heads.insert("workspace".to_owned(), 400);
    heads.insert(crate::thread_stream(&thread_id()), 1850);
    vec![
        ServerNotification::StreamBatch(StreamBatch {
            stream: crate::thread_stream(&thread_id()),
            head: 1850,
            events: events()
                .into_iter()
                .filter(|e| !e.event.is_workspace_event())
                .take(3)
                .collect(),
        }),
        // Nothing is left between the subscription's position and the head (the events were
        // deleted by retention): the client moves its position to `head`.
        ServerNotification::StreamBatch(StreamBatch {
            stream: "workspace".to_owned(),
            head: 400,
            events: Vec::new(),
        }),
        ServerNotification::Heartbeat(Heartbeat {
            server_time: T0 + 99_000,
            heads,
        }),
        ServerNotification::ConnectionReplaced(ConnectionReplaced {}),
        ServerNotification::ServerShuttingDown(ServerShuttingDown {
            reason: ShutdownReason::Drain,
            restart_expected: true,
        }),
    ]
}

/// A golden fixture file: path relative to `fixtures/protocol/` and its JSON content.
pub struct Fixture {
    pub path: String,
    pub value: Value,
}

fn file_name(name: &str) -> String {
    name.replace('/', "_")
}

/// Every golden fixture.
pub fn fixtures() -> Vec<Fixture> {
    let mut out = Vec::new();
    for (i, (req, result)) in request_examples().into_iter().enumerate() {
        let id = RequestId::Number(i as i64 + 1);
        let request = RpcMessage::request(id.clone(), req.method(), req.params_json());
        out.push(Fixture {
            path: format!("requests/{}.json", file_name(req.method())),
            value: serde_json::to_value(&request).unwrap(),
        });
        let response = RpcMessage::response_ok(id, result);
        out.push(Fixture {
            path: format!("responses/{}.json", file_name(req.method())),
            value: serde_json::to_value(&response).unwrap(),
        });
    }
    for env in events() {
        let name = if env.seq_from.is_some() {
            format!("{}_coalesced", file_name(env.event.type_name()))
        } else {
            file_name(env.event.type_name())
        };
        out.push(Fixture {
            path: format!("events/{name}.json"),
            value: serde_json::to_value(&env).unwrap(),
        });
    }
    for note in notifications() {
        let msg = RpcMessage::notification(note.method(), note.params_json());
        let name = match &note {
            ServerNotification::StreamBatch(batch) if batch.events.is_empty() => {
                format!("{}_empty", file_name(note.method()))
            }
            _ => file_name(note.method()),
        };
        out.push(Fixture {
            path: format!("notifications/{name}.json"),
            value: serde_json::to_value(&msg).unwrap(),
        });
    }
    for kind in ErrorKind::ALL {
        let mut err = RpcError::new(kind, format!("example {kind}"));
        if kind == ErrorKind::HarnessUnavailable {
            // Which harness a refused request waits for, and why (protocol.md §1.3).
            err = err
                .with("harnessId", "codex")
                .with("reason", "not logged in");
        }
        let msg = RpcMessage::response_err(Some(RequestId::Number(99)), err);
        out.push(Fixture {
            path: format!("errors/{}.json", kind.as_str()),
            value: serde_json::to_value(&msg).unwrap(),
        });
    }
    let http: Vec<(&str, Value)> = vec![
        (
            "pair_request",
            serde_json::to_value(PairRequest {
                code: "7K3M-Q9TX".into(),
                device_name: "Pixel".into(),
                platform: "android".into(),
            })
            .unwrap(),
        ),
        (
            "pair_response",
            serde_json::to_value(PairResponse {
                device_id: device_id(),
                token: "k8Zq1xRZr8nq2m3p4o5i6u7y8t9r0e1w2q3a4s5d6f7".into(),
                server: PairServerInfo {
                    name: "home-pc".into(),
                    epoch: "01K6A0000000000000000EPOCH".into(),
                },
            })
            .unwrap(),
        ),
        (
            "blob_upload_response",
            serde_json::to_value(BlobUploadResponse {
                blob_id: blob_id(),
                mime: "image/png".into(),
                size: 48_213,
            })
            .unwrap(),
        ),
        (
            "http_error",
            serde_json::to_value(HttpError {
                kind: "unauthorized".into(),
                message: "invalid token".into(),
            })
            .unwrap(),
        ),
        ("health", serde_json::to_value(Health { ok: true }).unwrap()),
    ];
    for (name, value) in http {
        out.push(Fixture {
            path: format!("http/{name}.json"),
            value,
        });
    }
    out
}

/// Tiny object-safe serialization helper so heterogeneous results fit in one `vec!`.
mod erased {
    pub trait Ser {
        fn to_value(&self) -> serde_json::Value;
    }
    impl<T: serde::Serialize> Ser for T {
        fn to_value(&self) -> serde_json::Value {
            serde_json::to_value(self).expect("example serializes")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_method_has_an_example() {
        let methods: Vec<&str> = all_requests().iter().map(|r| r.method()).collect();
        assert_eq!(methods, crate::METHOD_NAMES.to_vec());
    }

    #[test]
    fn every_event_type_has_an_example() {
        let types: std::collections::HashSet<&str> =
            events().iter().map(|e| e.event.type_name()).collect();
        // Keep this list in sync with `Event`.
        for t in [
            "project/upserted",
            "project/removed",
            "thread/upserted",
            "thread/removed",
            "interaction/pending",
            "interaction/closed",
            "harness/updated",
            "operation/updated",
            "thread/updated",
            "turn/started",
            "turn/completed",
            "turn/diffUpdated",
            "turn/usageUpdated",
            "item/started",
            "item/delta",
            "item/updated",
            "item/completed",
            "interaction/requested",
            "interaction/resolved",
            "interaction/expired",
            "queue/updated",
            "commands/changed",
            "backgroundTask/updated",
            "thread/nativeSessionChanged",
            "composer/insert",
            "native",
        ] {
            assert!(types.contains(t), "missing example for {t}");
        }
    }
}
