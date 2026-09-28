//! Fixed mapping of pi's built-in tools to protocol items (shared by live turns and history).
//!
//! | pi tool              | item                         |
//! |----------------------|------------------------------|
//! | `bash`, `powershell` | `commandExecution`           |
//! | `edit`, `write`      | `fileChange` (update)        |
//! | `read`               | `toolCall` / `read`          |
//! | `grep`, `find`, `ls` | `toolCall` / `search`        |
//! | anything else        | `toolCall` / `other`         |

use aas_protocol::{FileChange, FileChangeKind, ItemBody, ToolCategory};
use serde_json::Value;

use crate::wire::content_text;

/// How a tool's item evolves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolKind {
    Command,
    FileChange,
    Generic,
}

fn str_arg<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}

/// The item body when a tool call starts.
pub fn start_body(tool: &str, args: &Value) -> (ItemBody, ToolKind) {
    match tool {
        "bash" | "powershell" => (
            ItemBody::CommandExecution {
                command: str_arg(args, "command").unwrap_or_default().to_owned(),
                cwd: None,
                output: String::new(),
                output_truncated: false,
                output_blob_id: None,
                exit_code: None,
                duration_ms: None,
            },
            ToolKind::Command,
        ),
        "edit" | "write" => (
            ItemBody::FileChange {
                changes: vec![FileChange {
                    path: str_arg(args, "path").unwrap_or_default().to_owned(),
                    kind: FileChangeKind::Update,
                    move_path: None,
                    diff: None,
                    added: None,
                    removed: None,
                }],
            },
            ToolKind::FileChange,
        ),
        _ => {
            let (category, title) = match tool {
                "read" => (
                    ToolCategory::Read,
                    format!("Read {}", str_arg(args, "path").unwrap_or_default()),
                ),
                "grep" => (
                    ToolCategory::Search,
                    format!(
                        "Search for {}",
                        str_arg(args, "pattern").unwrap_or_default()
                    ),
                ),
                "find" => (
                    ToolCategory::Search,
                    format!("Find {}", str_arg(args, "pattern").unwrap_or_default()),
                ),
                "ls" => (
                    ToolCategory::Search,
                    format!("List {}", str_arg(args, "path").unwrap_or(".")),
                ),
                other => (ToolCategory::Other, other.to_owned()),
            };
            (
                ItemBody::ToolCall {
                    category,
                    name: tool.to_owned(),
                    title,
                    server: None,
                    input: Some(args.clone()),
                    output: None,
                    output_truncated: false,
                    output_blob_id: None,
                },
                ToolKind::Generic,
            )
        }
    }
}

/// Replaces the output of a command/generic body (streamed progress).
pub fn with_output(body: &ItemBody, text: &str) -> ItemBody {
    let mut body = body.clone();
    match &mut body {
        ItemBody::CommandExecution { output, .. } => *output = text.to_owned(),
        ItemBody::ToolCall { output, .. } => *output = Some(text.to_owned()),
        _ => {}
    }
    body
}

/// The item body when a tool call ends. `result` is pi's `{content, details}`.
pub fn final_body(start: &ItemBody, kind: ToolKind, result: &Value) -> ItemBody {
    let text = result.get("content").map(content_text).unwrap_or_default();
    let details = result.get("details").unwrap_or(&Value::Null);
    let truncated = details.get("truncation").is_some_and(Value::is_object);
    let mut body = start.clone();
    match (&mut body, kind) {
        (
            ItemBody::CommandExecution {
                output,
                output_truncated,
                ..
            },
            ToolKind::Command,
        ) => {
            *output = text;
            *output_truncated = truncated;
        }
        (ItemBody::FileChange { changes }, ToolKind::FileChange) => {
            if let (Some(change), Some(patch)) = (
                changes.first_mut(),
                details.get("patch").and_then(Value::as_str),
            ) {
                let (added, removed) = count_patch_lines(patch);
                change.diff = Some(patch.to_owned());
                change.added = Some(added);
                change.removed = Some(removed);
            }
        }
        (
            ItemBody::ToolCall {
                output,
                output_truncated,
                ..
            },
            ToolKind::Generic,
        ) => {
            *output = Some(text);
            *output_truncated = truncated;
        }
        _ => {}
    }
    body
}

/// Counts added/removed lines of a unified diff (file headers excluded).
pub fn count_patch_lines(patch: &str) -> (u64, u64) {
    let mut added = 0;
    let mut removed = 0;
    for line in patch.lines() {
        if line.starts_with("+++") || line.starts_with("---") {
            continue;
        }
        if line.starts_with('+') {
            added += 1;
        } else if line.starts_with('-') {
            removed += 1;
        }
    }
    (added, removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn shell_tools_are_commands() {
        for tool in ["bash", "powershell"] {
            let (body, kind) = start_body(tool, &json!({"command": "echo hi"}));
            assert_eq!(kind, ToolKind::Command);
            assert!(
                matches!(body, ItemBody::CommandExecution { ref command, .. } if command == "echo hi")
            );
        }
    }

    #[test]
    fn edit_final_body_carries_patch_and_counts() {
        let (start, kind) = start_body("edit", &json!({"path": "src/a.rs", "edits": []}));
        let patch = "--- a/src/a.rs\n+++ b/src/a.rs\n@@ -1,2 +1,2 @@\n-old\n+new\n+more\n ctx\n";
        let body = final_body(
            &start,
            kind,
            &json!({"content": [{"type":"text","text":"ok"}], "details": {"patch": patch}}),
        );
        match body {
            ItemBody::FileChange { changes } => {
                assert_eq!(changes[0].path, "src/a.rs");
                assert_eq!(changes[0].added, Some(2));
                assert_eq!(changes[0].removed, Some(1));
                assert_eq!(changes[0].diff.as_deref(), Some(patch));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn categories_of_read_and_search_tools() {
        let cases = [
            ("read", ToolCategory::Read, "Read a.txt"),
            ("grep", ToolCategory::Search, "Search for foo"),
            ("find", ToolCategory::Search, "Find *.rs"),
            ("ls", ToolCategory::Search, "List ."),
            ("ask_question", ToolCategory::Other, "ask_question"),
        ];
        for (tool, cat, title) in cases {
            let (body, kind) = start_body(
                tool,
                &json!({"path": "a.txt", "pattern": if tool == "find" {"*.rs"} else {"foo"}}),
            );
            assert_eq!(kind, ToolKind::Generic);
            match body {
                ItemBody::ToolCall {
                    category, title: t, ..
                } => {
                    assert_eq!(category, cat, "{tool}");
                    if tool != "ls" {
                        assert_eq!(t, title, "{tool}");
                    }
                }
                other => panic!("{other:?}"),
            }
        }
    }

    #[test]
    fn command_truncation_is_structural() {
        let (start, kind) = start_body("bash", &json!({"command": "big"}));
        let body = final_body(
            &start,
            kind,
            &json!({"content": [{"type":"text","text":"tail"}], "details": {"truncation": {"lines": 10}, "fullOutputPath": "x"}}),
        );
        assert!(
            matches!(body, ItemBody::CommandExecution { output_truncated: true, ref output, .. } if output == "tail")
        );
    }
}
