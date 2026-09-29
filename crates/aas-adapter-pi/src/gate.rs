//! The approval gate: a pi extension we ship, plus the mapping of pi's extension UI
//! dialogs (`extension_ui_request`) to interactions.
//!
//! The gate and the adapter talk through a fixed, machine-readable format (see
//! `extension/aas-gate.ts`): the dialog title is `aas-gate:` + JSON, the answer is a JSON
//! string, and the gate reports how each of its dialogs closed, and why its fork command did
//! not fork, with a `notify` whose message is `aas-gate:` + JSON. Dialogs opened by other
//! extensions are mapped by their `method` alone.
//!
//! The gate also registers two commands: `/reload` (pi's `ctx.reload()`, which pi's RPC mode
//! has no command for) and [`FORK_COMMAND`] (the adapter's way to fork at any entry).

use std::path::{Path, PathBuf};

use aas_protocol::{
    ApprovalOption, ApprovalOptionKind, FileChange, FileChangeKind, InteractionRequest,
    InteractionResolution, Question, QuestionChoice, Subject,
};
use serde_json::{Value, json};

/// Source of the gate extension (written to the adapter's state dir and loaded with `-e`).
pub const EXTENSION_SOURCE: &str = include_str!("../extension/aas-gate.ts");
/// File name of the installed extension (versioned so an upgrade never loads a stale copy).
/// v2: the gate reports the closure of its dialogs (`dialogClosed`). v3: the commands
/// `/reload` and [`FORK_COMMAND`], and the report `forkFailed`.
pub const EXTENSION_FILE: &str = "aas-gate-v3.ts";
/// The gate's command that forks the session: `/aas-gate-fork <entryId> <at|before>`
/// (`ctx.fork(entryId, { position })`). Sent by the adapter only; never offered.
pub const FORK_COMMAND: &str = "aas-gate-fork";
/// Environment variable naming the mode file.
pub const MODE_FILE_ENV: &str = "AAS_PI_GATE_FILE";
const TITLE_PREFIX: &str = "aas-gate:";

/// Permission modes of pi threads.
pub const MODE_ASK: &str = "ask";
pub const MODE_ASK_COMMANDS: &str = "askCommands";
pub const MODE_AUTO: &str = "auto";
pub const MODES: [&str; 3] = [MODE_ASK, MODE_ASK_COMMANDS, MODE_AUTO];

pub fn is_valid_mode(mode: &str) -> bool {
    MODES.contains(&mode)
}

/// Writes the extension into `dir` (only when its content differs) and returns its path.
pub fn install_extension(dir: &Path) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(EXTENSION_FILE);
    if std::fs::read(&path).ok().as_deref() != Some(EXTENSION_SOURCE.as_bytes()) {
        write_atomic(&path, EXTENSION_SOURCE.as_bytes())?;
    }
    Ok(path)
}

/// Writes the permission mode read by the extension on every tool call.
pub fn write_mode(path: &Path, mode: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    write_atomic(path, json!({ "mode": mode }).to_string().as_bytes())
}

fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension(format!("tmp-{}", uuid::Uuid::new_v4().simple()));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// A dialog pi is waiting on, remembered until it is answered.
#[derive(Debug, Clone, PartialEq)]
pub enum PendingDialog {
    Gate { tool_call_id: String, shell: bool },
    Select { options: Vec<String> },
    Confirm,
    Input,
    Editor { prefill: String },
}

/// Option ids offered for gate approvals.
pub const OPT_ALLOW: &str = "allow";
pub const OPT_ALLOW_SESSION: &str = "allowSession";
pub const OPT_DENY: &str = "deny";
pub const OPT_DENY_FEEDBACK: &str = "denyWithFeedback";

/// Interpretation of an `extension_ui_request`.
#[derive(Debug, Clone, PartialEq)]
pub enum Dialog {
    /// Needs an answer: request to publish and what to remember.
    Ask {
        request: InteractionRequest,
        pending: PendingDialog,
        tool_call_id: Option<String>,
    },
    /// `notify`: shown as a notice.
    Notify {
        level: aas_protocol::NoticeLevel,
        message: String,
    },
    /// The gate's report that its dialog for a tool call closed. `aborted` means pi closed it
    /// because the turn was aborted, so no answer of ours was used.
    GateClosed { tool_call_id: String, aborted: bool },
    /// The gate's report that [`FORK_COMMAND`] did not fork (pi's error, or a cancellation).
    ForkFailed { error: String },
    /// `set_editor_text` (an extension's `setEditorText` or `pasteToEditor`): text for the
    /// composer.
    EditorText { text: String },
    /// TUI-only fire-and-forget methods (`setStatus`, `setWidget`, `setTitle`).
    Ignored,
    /// Unknown method.
    Unknown,
}

/// Text added to a dialog that carries a `timeout`: pi resolves such a dialog with its
/// default when the time is up and does not tell the client, so the user is told up front.
fn timeout_note(req: &Value) -> Option<String> {
    let ms = req.get("timeout").and_then(Value::as_u64)?;
    Some(format!(
        "pi answers this dialog with its default after {} s without a reply.",
        ms.div_ceil(1000)
    ))
}

fn with_note(text: &str, note: Option<&str>) -> String {
    match note {
        Some(note) if text.is_empty() => note.to_owned(),
        Some(note) => format!("{text}\n\n{note}"),
        None => text.to_owned(),
    }
}

/// Maps an `extension_ui_request` (without deciding anything heuristically: the gate is
/// recognised by its exact title or message prefix, other dialogs by `method`).
pub fn interpret(req: &Value) -> Dialog {
    let method = req
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let title = req.get("title").and_then(Value::as_str).unwrap_or_default();
    let note = timeout_note(req);
    let note = note.as_deref();
    match method {
        "select" => {
            if let Some(gate) = title
                .strip_prefix(TITLE_PREFIX)
                .and_then(|j| serde_json::from_str::<Value>(j).ok())
            {
                return gate_dialog(&gate);
            }
            let options: Vec<String> = req
                .get("options")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default();
            let choices = options
                .iter()
                .enumerate()
                .map(|(i, label)| QuestionChoice {
                    id: i.to_string(),
                    label: label.clone(),
                    description: None,
                })
                .collect();
            Dialog::Ask {
                request: InteractionRequest::Question {
                    title: title.to_owned(),
                    questions: vec![Question {
                        id: "answer".into(),
                        header: None,
                        prompt: with_note(title, note),
                        choices,
                        multi_select: false,
                        allow_free_text: false,
                        placeholder: None,
                    }],
                },
                pending: PendingDialog::Select { options },
                tool_call_id: None,
            }
        }
        "confirm" => {
            let message = req
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or_default();
            Dialog::Ask {
                request: InteractionRequest::Approval {
                    title: title.to_owned(),
                    detail: Some(with_note(message, note)).filter(|d| !d.is_empty()),
                    subject: Subject::Other {
                        description: if message.is_empty() {
                            title.to_owned()
                        } else {
                            message.to_owned()
                        },
                    },
                    options: vec![
                        ApprovalOption {
                            id: "yes".into(),
                            label: "Yes".into(),
                            kind: ApprovalOptionKind::AllowOnce,
                        },
                        ApprovalOption {
                            id: "no".into(),
                            label: "No".into(),
                            kind: ApprovalOptionKind::Deny,
                        },
                    ],
                },
                pending: PendingDialog::Confirm,
                tool_call_id: None,
            }
        }
        "input" | "editor" => {
            let prefill = req
                .get("prefill")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let placeholder = req
                .get("placeholder")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let prompt = if method == "editor" && !prefill.is_empty() {
                format!("{title}\n\n{prefill}")
            } else {
                title.to_owned()
            };
            let prompt = with_note(&prompt, note);
            Dialog::Ask {
                request: InteractionRequest::Question {
                    title: title.to_owned(),
                    questions: vec![Question {
                        id: "answer".into(),
                        header: None,
                        prompt,
                        choices: Vec::new(),
                        multi_select: false,
                        allow_free_text: true,
                        placeholder,
                    }],
                },
                pending: if method == "editor" {
                    PendingDialog::Editor { prefill }
                } else {
                    PendingDialog::Input
                },
                tool_call_id: None,
            }
        }
        "notify" => {
            let message = req
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if let Some(report) = message.strip_prefix(TITLE_PREFIX) {
                return gate_report(report);
            }
            let level = match req.get("notifyType").and_then(Value::as_str) {
                Some("warning") => aas_protocol::NoticeLevel::Warning,
                Some("error") => aas_protocol::NoticeLevel::Error,
                _ => aas_protocol::NoticeLevel::Info,
            };
            Dialog::Notify {
                level,
                message: message.to_owned(),
            }
        }
        "set_editor_text" => match req.get("text").and_then(Value::as_str) {
            Some(text) => Dialog::EditorText {
                text: text.to_owned(),
            },
            None => Dialog::Unknown,
        },
        "setStatus" | "setWidget" | "setTitle" => Dialog::Ignored,
        _ => Dialog::Unknown,
    }
}

/// A `notify` from the gate (`aas-gate:` + JSON): `dialogClosed` or `forkFailed`; anything else
/// is not ours to interpret.
fn gate_report(json: &str) -> Dialog {
    let Ok(report) = serde_json::from_str::<Value>(json) else {
        return Dialog::Unknown;
    };
    match report.get("event").and_then(Value::as_str) {
        Some("dialogClosed") => match report.get("toolCallId").and_then(Value::as_str) {
            Some(id) => Dialog::GateClosed {
                tool_call_id: id.to_owned(),
                aborted: report.get("reason").and_then(Value::as_str) == Some("aborted"),
            },
            None => Dialog::Unknown,
        },
        Some("forkFailed") => Dialog::ForkFailed {
            error: report
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("the gate did not say why")
                .to_owned(),
        },
        _ => Dialog::Unknown,
    }
}

fn gate_dialog(gate: &Value) -> Dialog {
    let tool_call_id = gate
        .get("toolCallId")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let tool = gate
        .get("toolName")
        .and_then(Value::as_str)
        .unwrap_or("tool");
    let input = gate.get("input").cloned().unwrap_or(Value::Null);
    let shell = matches!(tool, "bash" | "powershell");
    let (title, subject, session_label) = if shell {
        let command = input
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        (
            "Run command?".to_owned(),
            Subject::Command { command, cwd: None },
            "Allow this command for the session",
        )
    } else if matches!(tool, "edit" | "write") {
        let path = input
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        (
            format!("{} {path}?", if tool == "write" { "Write" } else { "Edit" }),
            Subject::FileChange {
                changes: vec![FileChange {
                    path,
                    kind: FileChangeKind::Update,
                    move_path: None,
                    diff: None,
                    added: None,
                    removed: None,
                }],
            },
            "Allow all edits for the session",
        )
    } else {
        (
            format!("Run {tool}?"),
            Subject::Tool {
                name: tool.to_owned(),
                input: Some(input),
            },
            "Allow for the session",
        )
    };
    Dialog::Ask {
        request: InteractionRequest::Approval {
            title,
            detail: None,
            subject,
            options: vec![
                ApprovalOption {
                    id: OPT_ALLOW.into(),
                    label: "Allow".into(),
                    kind: ApprovalOptionKind::AllowOnce,
                },
                ApprovalOption {
                    id: OPT_ALLOW_SESSION.into(),
                    label: session_label.into(),
                    kind: ApprovalOptionKind::AllowForSession,
                },
                ApprovalOption {
                    id: OPT_DENY.into(),
                    label: "Deny".into(),
                    kind: ApprovalOptionKind::Deny,
                },
                ApprovalOption {
                    id: OPT_DENY_FEEDBACK.into(),
                    label: "Deny with feedback".into(),
                    kind: ApprovalOptionKind::DenyWithFeedback,
                },
            ],
        },
        pending: PendingDialog::Gate {
            tool_call_id: tool_call_id.clone(),
            shell,
        },
        tool_call_id: (!tool_call_id.is_empty()).then_some(tool_call_id),
    }
}

/// The `extension_ui_response` body (without `type`/`id`) for a resolution, and whether a
/// gate request ended up denied.
#[derive(Debug, Clone, PartialEq)]
pub struct Answer {
    pub fields: Value,
    pub declined: bool,
}

/// Encodes a resolution for a pending dialog. Errors when the resolution does not fit.
pub fn answer(
    pending: &PendingDialog,
    resolution: &InteractionResolution,
) -> Result<Answer, String> {
    let cancelled = Answer {
        fields: json!({ "cancelled": true }),
        declined: true,
    };
    match (pending, resolution) {
        (_, InteractionResolution::Dismissed) => Ok(cancelled),
        (
            PendingDialog::Gate { .. },
            InteractionResolution::Approval {
                option_id,
                feedback,
            },
        ) => {
            let (choice, feedback) = match option_id.as_str() {
                OPT_ALLOW => ("allow", None),
                OPT_ALLOW_SESSION => ("allowSession", None),
                OPT_DENY => ("deny", None),
                OPT_DENY_FEEDBACK => ("deny", feedback.clone().filter(|f| !f.is_empty())),
                other => return Err(format!("unknown option {other}")),
            };
            let mut value = json!({ "choice": choice });
            if let Some(f) = feedback {
                value["feedback"] = json!(f);
            }
            Ok(Answer {
                fields: json!({ "value": value.to_string() }),
                declined: choice == "deny",
            })
        }
        (PendingDialog::Confirm, InteractionResolution::Approval { option_id, .. }) => {
            match option_id.as_str() {
                "yes" => Ok(Answer {
                    fields: json!({ "confirmed": true }),
                    declined: false,
                }),
                "no" => Ok(Answer {
                    fields: json!({ "confirmed": false }),
                    declined: false,
                }),
                other => Err(format!("unknown option {other}")),
            }
        }
        (PendingDialog::Select { options }, InteractionResolution::Question { answers }) => {
            let choice = answers.first().and_then(|a| a.choice_ids.first());
            match choice
                .and_then(|c| c.parse::<usize>().ok())
                .and_then(|i| options.get(i))
            {
                Some(value) => Ok(Answer {
                    fields: json!({ "value": value }),
                    declined: false,
                }),
                None => Err("the answer must pick one of the offered choices".into()),
            }
        }
        (PendingDialog::Input, InteractionResolution::Question { answers }) => {
            let text = answers
                .first()
                .and_then(|a| a.text.clone())
                .unwrap_or_default();
            Ok(Answer {
                fields: json!({ "value": text }),
                declined: false,
            })
        }
        (PendingDialog::Editor { prefill }, InteractionResolution::Question { answers }) => {
            // An empty answer keeps the prefilled text unchanged.
            let text = answers
                .first()
                .and_then(|a| a.text.clone())
                .filter(|t| !t.is_empty())
                .unwrap_or_else(|| prefill.clone());
            Ok(Answer {
                fields: json!({ "value": text }),
                declined: false,
            })
        }
        _ => Err("the resolution kind does not match the request".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aas_protocol::QuestionAnswer;

    fn gate_request(tool: &str, input: Value) -> Value {
        let payload = json!({"v":1,"toolCallId":"call_1","toolName":tool,"input":input});
        json!({"type":"extension_ui_request","id":"u1","method":"select","title":format!("aas-gate:{payload}"),"options":["allow","allowSession","deny"]})
    }

    #[test]
    fn gate_requests_become_approvals() {
        match interpret(&gate_request("bash", json!({"command":"echo hi"}))) {
            Dialog::Ask {
                request:
                    InteractionRequest::Approval {
                        subject, options, ..
                    },
                pending,
                tool_call_id,
            } => {
                assert_eq!(
                    subject,
                    Subject::Command {
                        command: "echo hi".into(),
                        cwd: None
                    }
                );
                assert_eq!(options.len(), 4);
                assert_eq!(
                    pending,
                    PendingDialog::Gate {
                        tool_call_id: "call_1".into(),
                        shell: true
                    }
                );
                assert_eq!(tool_call_id.as_deref(), Some("call_1"));
            }
            other => panic!("{other:?}"),
        }
        match interpret(&gate_request(
            "write",
            json!({"path":"a.txt","content":"x"}),
        )) {
            Dialog::Ask {
                request:
                    InteractionRequest::Approval {
                        subject: Subject::FileChange { changes },
                        ..
                    },
                ..
            } => {
                assert_eq!(changes[0].path, "a.txt")
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn gate_answers_encode_choice_and_feedback() {
        let pending = PendingDialog::Gate {
            tool_call_id: "c".into(),
            shell: true,
        };
        let a = answer(
            &pending,
            &InteractionResolution::Approval {
                option_id: "allow".into(),
                feedback: None,
            },
        )
        .unwrap();
        assert_eq!(a.fields, json!({"value": "{\"choice\":\"allow\"}"}));
        assert!(!a.declined);
        let d = answer(
            &pending,
            &InteractionResolution::Approval {
                option_id: "denyWithFeedback".into(),
                feedback: Some("not now".into()),
            },
        )
        .unwrap();
        let inner: Value = serde_json::from_str(d.fields["value"].as_str().unwrap()).unwrap();
        assert_eq!(inner, json!({"choice":"deny","feedback":"not now"}));
        assert!(d.declined);
        let c = answer(&pending, &InteractionResolution::Dismissed).unwrap();
        assert_eq!(c.fields, json!({"cancelled": true}));
        assert!(
            answer(
                &pending,
                &InteractionResolution::Approval {
                    option_id: "bogus".into(),
                    feedback: None
                }
            )
            .is_err()
        );
    }

    #[test]
    fn other_dialogs_map_by_method() {
        let select = json!({"method":"select","id":"x","title":"Pick","options":["A","B"]});
        let Dialog::Ask { pending, .. } = interpret(&select) else {
            panic!()
        };
        let res = InteractionResolution::Question {
            answers: vec![QuestionAnswer {
                question_id: "answer".into(),
                choice_ids: vec!["1".into()],
                text: None,
            }],
        };
        assert_eq!(answer(&pending, &res).unwrap().fields, json!({"value":"B"}));

        let confirm = json!({"method":"confirm","id":"x","title":"Clear?","message":"All lost"});
        let Dialog::Ask {
            pending, request, ..
        } = interpret(&confirm)
        else {
            panic!()
        };
        assert!(matches!(request, InteractionRequest::Approval { .. }));
        let yes = InteractionResolution::Approval {
            option_id: "yes".into(),
            feedback: None,
        };
        assert_eq!(
            answer(&pending, &yes).unwrap().fields,
            json!({"confirmed": true})
        );

        let editor = json!({"method":"editor","id":"x","title":"Edit","prefill":"abc"});
        let Dialog::Ask { pending, .. } = interpret(&editor) else {
            panic!()
        };
        let empty = InteractionResolution::Question {
            answers: vec![QuestionAnswer {
                question_id: "answer".into(),
                choice_ids: vec![],
                text: Some(String::new()),
            }],
        };
        assert_eq!(
            answer(&pending, &empty).unwrap().fields,
            json!({"value":"abc"})
        );

        // Timed dialogs say so (pi closes them by itself and does not tell the client).
        let timed =
            json!({"method":"select","id":"x","title":"Pick","options":["A"],"timeout":10000});
        let Dialog::Ask {
            request: InteractionRequest::Question { questions, .. },
            ..
        } = interpret(&timed)
        else {
            panic!()
        };
        assert_eq!(
            questions[0].prompt,
            "Pick\n\npi answers this dialog with its default after 10 s without a reply."
        );
        let timed = json!({"method":"confirm","id":"x","title":"Clear?","timeout":1500});
        let Dialog::Ask {
            request: InteractionRequest::Approval { detail, .. },
            ..
        } = interpret(&timed)
        else {
            panic!()
        };
        assert_eq!(
            detail.as_deref(),
            Some("pi answers this dialog with its default after 2 s without a reply.")
        );

        assert_eq!(interpret(&json!({"method":"setStatus"})), Dialog::Ignored);
        // Recorded from pi 0.85.1 (`ctx.ui.setEditorText`, also `pasteToEditor`).
        assert_eq!(
            interpret(
                &json!({"type":"extension_ui_request","id":"e1","method":"set_editor_text","text":"Hello from the extension"})
            ),
            Dialog::EditorText {
                text: "Hello from the extension".into()
            }
        );
        assert_eq!(
            interpret(&json!({"method":"set_editor_text"})),
            Dialog::Unknown
        );
        assert_eq!(
            interpret(&json!({"method":"somethingNew"})),
            Dialog::Unknown
        );
        assert!(matches!(
            interpret(&json!({"method":"notify","message":"hi","notifyType":"warning"})),
            Dialog::Notify {
                level: aas_protocol::NoticeLevel::Warning,
                ..
            }
        ));
    }

    #[test]
    fn gate_closure_reports_are_recognised() {
        let report = |r: Value| json!({"type":"extension_ui_request","id":"n1","method":"notify","notifyType":"info","message":format!("aas-gate:{r}")});
        assert_eq!(
            interpret(&report(
                json!({"v":1,"event":"dialogClosed","toolCallId":"call_1","reason":"aborted"})
            )),
            Dialog::GateClosed {
                tool_call_id: "call_1".into(),
                aborted: true
            }
        );
        assert_eq!(
            interpret(&report(
                json!({"v":1,"event":"dialogClosed","toolCallId":"call_2","reason":"answered"})
            )),
            Dialog::GateClosed {
                tool_call_id: "call_2".into(),
                aborted: false
            }
        );
        assert_eq!(
            interpret(&report(json!({"v":1,"event":"somethingElse"}))),
            Dialog::Unknown
        );
        assert_eq!(
            interpret(&report(
                json!({"v":1,"event":"forkFailed","error":"Invalid entry ID for forking"})
            )),
            Dialog::ForkFailed {
                error: "Invalid entry ID for forking".into()
            }
        );
        assert_eq!(
            interpret(&report(json!({"v":1,"event":"dialogClosed"}))),
            Dialog::Unknown,
            "a closure names its tool call"
        );
        let broken = json!({"method":"notify","message":"aas-gate:{oops"});
        assert_eq!(interpret(&broken), Dialog::Unknown);
    }

    #[test]
    fn extension_and_mode_files_are_written() {
        let dir = tempfile::tempdir().unwrap();
        let path = install_extension(dir.path()).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), EXTENSION_SOURCE);
        // idempotent
        install_extension(dir.path()).unwrap();
        let mode = dir.path().join("gate").join("s.json");
        write_mode(&mode, MODE_AUTO).unwrap();
        write_mode(&mode, MODE_ASK).unwrap();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&mode).unwrap()).unwrap();
        assert_eq!(v, json!({"mode":"ask"}));
    }
}
