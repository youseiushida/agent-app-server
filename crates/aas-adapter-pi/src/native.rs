//! pi session files: discovery and history import.
//!
//! File format: `docs/session-format.md` of pi (v3). The first line is the header
//! (`{"type":"session","id","cwd",…}`); entries form a tree through `id`/`parentId`, and the
//! active branch is the path from the last entry in file order to the root (pi's own rule in
//! `SessionManager._buildIndex`).
//!
//! Failures are reported, never turned into an empty result: a session file that cannot be
//! read (or whose first line is not JSON) is skipped and returned as unreadable with its path;
//! a later line that is not JSON (e.g. the half-written last line of a session pi is writing
//! right now) is skipped and logged with its path and line number. A JSON file whose first
//! line is not a pi session header is not a pi session and is left out.

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use aas_harness::{
    AdapterError, AdapterPolicy, HistoryItem, HistoryTurn, ItemBody, ItemStatus, Millis,
    NativeHistory, NativeSessionScan, NativeSessionSummary, NoticeLevel,
};
use aas_protocol::UserMessageDelivery;
use serde_json::Value;

use crate::paths::{PathInputs, same_dir, unreadable};
use crate::tools::{self, ToolKind};
use crate::wire::content_text;

/// Header of a session file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    pub id: String,
    pub cwd: String,
}

/// Reads the header (first non-empty line) of a session file. `Ok(None)`: the file is empty or
/// its first line is JSON but not a pi session header (not a pi session). A read error or a
/// first line that is not JSON is an error.
pub fn read_header(path: &Path) -> std::io::Result<Option<Header>> {
    let file = std::fs::File::open(path)?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        if !line.trim().is_empty() {
            break;
        }
    }
    let value: Value = serde_json::from_str(line.trim()).map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("the first line is not JSON: {e}"),
        )
    })?;
    if value.get("type").and_then(Value::as_str) != Some("session") {
        return Ok(None);
    }
    let Some(id) = value.get("id").and_then(Value::as_str) else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "the session header has no id",
        ));
    };
    Ok(Some(Header {
        id: id.to_owned(),
        cwd: value
            .get("cwd")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
    }))
}

/// Finds the file of a session id (ids are UUIDs, unique across projects). `Ok(None)` when no
/// readable file has it; when some files could not be read, the session may be one of them, so
/// that is an error naming them.
pub fn find_session_file(
    inputs: &PathInputs,
    cwd: &Path,
    id: &str,
) -> Result<Option<PathBuf>, AdapterError> {
    let candidates = inputs.candidate_files(cwd)?;
    let mut skipped = candidates.unreadable;
    for path in candidates.files {
        match read_header(&path) {
            Ok(Some(header)) if header.id == id => return Ok(Some(path)),
            Ok(_) => {}
            Err(e) => skipped.push(unreadable(&path, e)),
        }
    }
    if skipped.is_empty() {
        return Ok(None);
    }
    let list: Vec<String> = skipped
        .iter()
        .map(|s| format!("{} ({})", s.location, s.error))
        .collect();
    Err(AdapterError::Other(format!(
        "pi session {id} was not found, and these session files could not be read: {}",
        list.join("; ")
    )))
}

/// Sessions whose header `cwd` is `cwd`, most recently active first, and the files (or
/// folders) that could not be read.
pub fn list_sessions(
    inputs: &PathInputs,
    cwd: &Path,
    policy: &AdapterPolicy,
) -> Result<NativeSessionScan, AdapterError> {
    let candidates = inputs.candidate_files(cwd)?;
    let mut skipped = candidates.unreadable;
    let mut out = Vec::new();
    for path in candidates.files {
        let header = match read_header(&path) {
            Ok(Some(header)) => header,
            Ok(None) => continue,
            Err(e) => {
                skipped.push(unreadable(&path, e));
                continue;
            }
        };
        if !same_dir(Path::new(&header.cwd), cwd) {
            continue;
        }
        let lines = match read_lines(&path) {
            Ok(lines) => lines,
            Err(e) => {
                skipped.push(unreadable(&path, e));
                continue;
            }
        };
        let (title, last_activity) = summarize(&lines, policy);
        let mtime = match std::fs::metadata(&path).and_then(|m| m.modified()) {
            Ok(t) => t
                .duration_since(std::time::UNIX_EPOCH)
                .ok()
                .map(|d| d.as_millis() as Millis),
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "cannot read the modification time of a pi session file");
                None
            }
        };
        out.push(NativeSessionSummary {
            native_session_id: header.id,
            title,
            updated_at: last_activity.or(mtime),
            cwd: Some(header.cwd),
        });
    }
    out.sort_by(|a, b| {
        b.updated_at
            .cmp(&a.updated_at)
            .then(a.native_session_id.cmp(&b.native_session_id))
    });
    Ok(NativeSessionScan {
        sessions: out,
        unreadable: skipped,
    })
}

/// The JSON entries of a session file, in order. Lines that are not JSON are skipped and
/// logged (pi may be writing the last line right now); a read error fails the whole file.
fn read_lines(path: &Path) -> std::io::Result<Vec<Value>> {
    let text = std::fs::read_to_string(path)?;
    let mut out = Vec::new();
    for (index, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str(line) {
            Ok(v) => out.push(v),
            Err(e) => tracing::warn!(
                path = %path.display(),
                line = index + 1,
                error = %e,
                "skipped a pi session line that is not JSON"
            ),
        }
    }
    Ok(out)
}

/// Title (latest `session_info` name, else the first line of the first user message) and the
/// latest message timestamp.
/// Title and last activity of a session. The title is its name (`session_info`, cut to
/// `policy.harness_title_chars`), else the first line of its first user message (cut to
/// `policy.first_message_title_chars`, the engine's rule for titles made from a first message).
fn summarize(entries: &[Value], policy: &AdapterPolicy) -> (Option<String>, Option<Millis>) {
    let mut name: Option<String> = None;
    let mut first_user: Option<String> = None;
    let mut last: Option<Millis> = None;
    for e in entries {
        match e.get("type").and_then(Value::as_str) {
            Some("session_info") => {
                name = e
                    .get("name")
                    .and_then(Value::as_str)
                    .and_then(|n| policy.harness_title(n));
            }
            Some("message") => {
                if let Some(ts) = e.pointer("/message/timestamp").and_then(Value::as_i64) {
                    last = Some(last.map_or(ts, |l: Millis| l.max(ts)));
                }
                if first_user.is_none()
                    && e.pointer("/message/role").and_then(Value::as_str) == Some("user")
                {
                    let text = content_text(e.pointer("/message/content").unwrap_or(&Value::Null));
                    first_user = policy.prompt_title(&text);
                }
            }
            _ => {}
        }
    }
    (name.or(first_user), last)
}

/// Full history of the session file at `path` (active branch only).
pub fn read_history(path: &Path, policy: &AdapterPolicy) -> std::io::Result<NativeHistory> {
    let entries = read_lines(path)?;
    Ok(history_from_entries(&entries, policy))
}

/// Builds the history from parsed entries (header included or not).
pub fn history_from_entries(entries: &[Value], policy: &AdapterPolicy) -> NativeHistory {
    let (title, _) = summarize(entries, policy);
    let tree: Vec<&Value> = entries
        .iter()
        .filter(|e| e.get("type").and_then(Value::as_str) != Some("session"))
        .collect();
    let by_id: HashMap<&str, &Value> = tree
        .iter()
        .filter_map(|e| Some((e.get("id")?.as_str()?, *e)))
        .collect();

    // Active branch: from the last entry back to the root.
    let mut branch = Vec::new();
    let mut cursor = tree.last().copied();
    let mut guard = 0usize;
    while let Some(entry) = cursor {
        branch.push(entry);
        guard += 1;
        if guard > tree.len() {
            break; // cycle protection for corrupt files
        }
        cursor = entry
            .get("parentId")
            .and_then(Value::as_str)
            .and_then(|p| by_id.get(p).copied());
    }
    branch.reverse();

    let mut turns: Vec<HistoryTurn> = Vec::new();
    // Tool items awaiting their result: tool_call_id -> (turn index, item index, kind)
    let mut open_tools: HashMap<String, (usize, usize, ToolKind)> = HashMap::new();

    fn current(turns: &mut Vec<HistoryTurn>) -> &mut HistoryTurn {
        if turns.is_empty() {
            turns.push(HistoryTurn::default());
        }
        turns.last_mut().expect("non-empty")
    }

    for entry in branch {
        match entry.get("type").and_then(Value::as_str) {
            Some("message") => {
                let msg = &entry["message"];
                let ts = msg.get("timestamp").and_then(Value::as_i64);
                match msg.get("role").and_then(Value::as_str) {
                    Some("user") => {
                        turns.push(HistoryTurn {
                            started_at: ts,
                            completed_at: ts,
                            items: Vec::new(),
                        });
                        let text = content_text(msg.get("content").unwrap_or(&Value::Null));
                        current(&mut turns).items.push(HistoryItem {
                            body: ItemBody::UserMessage {
                                text,
                                attachments: Vec::new(),
                                mentions: Vec::new(),
                                delivery: UserMessageDelivery::Normal,
                            },
                            status: ItemStatus::Completed,
                        });
                    }
                    Some("assistant") => {
                        let aborted =
                            msg.get("stopReason").and_then(Value::as_str) == Some("aborted");
                        let status = if aborted {
                            ItemStatus::Interrupted
                        } else {
                            ItemStatus::Completed
                        };
                        let turn_index = {
                            current(&mut turns);
                            turns.len() - 1
                        };
                        let turn = &mut turns[turn_index];
                        if ts.is_some() {
                            turn.completed_at = ts;
                            turn.started_at = turn.started_at.or(ts);
                        }
                        for part in msg
                            .get("content")
                            .and_then(Value::as_array)
                            .into_iter()
                            .flatten()
                        {
                            match part.get("type").and_then(Value::as_str) {
                                Some("text") => turn.items.push(HistoryItem {
                                    body: ItemBody::AgentMessage {
                                        text: part["text"].as_str().unwrap_or_default().to_owned(),
                                    },
                                    status,
                                }),
                                Some("thinking") => turn.items.push(HistoryItem {
                                    body: ItemBody::Reasoning {
                                        text: part["thinking"]
                                            .as_str()
                                            .unwrap_or_default()
                                            .to_owned(),
                                    },
                                    status,
                                }),
                                Some("toolCall") => {
                                    let name =
                                        part.get("name").and_then(Value::as_str).unwrap_or("tool");
                                    let args =
                                        part.get("arguments").cloned().unwrap_or(Value::Null);
                                    let (body, kind) = tools::start_body(name, &args);
                                    if let Some(id) = part.get("id").and_then(Value::as_str) {
                                        open_tools.insert(
                                            id.to_owned(),
                                            (turn_index, turn.items.len(), kind),
                                        );
                                    }
                                    turn.items.push(HistoryItem {
                                        body,
                                        status: if aborted {
                                            ItemStatus::Interrupted
                                        } else {
                                            ItemStatus::InProgress
                                        },
                                    });
                                }
                                _ => {}
                            }
                        }
                    }
                    Some("toolResult") => {
                        let id = msg
                            .get("toolCallId")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        let is_error = msg.get("isError").and_then(Value::as_bool).unwrap_or(false);
                        let result = serde_json::json!({ "content": msg.get("content").cloned().unwrap_or(Value::Null), "details": msg.get("details").cloned().unwrap_or(Value::Null) });
                        let status = if is_error {
                            ItemStatus::Failed
                        } else {
                            ItemStatus::Completed
                        };
                        match open_tools.remove(id) {
                            Some((t, i, kind)) => {
                                let item = &mut turns[t].items[i];
                                item.body = tools::final_body(&item.body, kind, &result);
                                item.status = status;
                            }
                            None => {
                                let name = msg
                                    .get("toolName")
                                    .and_then(Value::as_str)
                                    .unwrap_or("tool");
                                let (start, kind) = tools::start_body(name, &Value::Null);
                                current(&mut turns).items.push(HistoryItem {
                                    body: tools::final_body(&start, kind, &result),
                                    status,
                                });
                            }
                        }
                    }
                    Some("bashExecution") => {
                        let output = msg
                            .get("output")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned();
                        current(&mut turns).items.push(HistoryItem {
                            body: ItemBody::CommandExecution {
                                command: msg
                                    .get("command")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default()
                                    .to_owned(),
                                cwd: None,
                                output,
                                output_truncated: msg
                                    .get("truncated")
                                    .and_then(Value::as_bool)
                                    .unwrap_or(false),
                                output_blob_id: None,
                                exit_code: msg
                                    .get("exitCode")
                                    .and_then(Value::as_i64)
                                    .map(|c| c as i32),
                                duration_ms: None,
                            },
                            status: if msg.get("cancelled").and_then(Value::as_bool) == Some(true) {
                                ItemStatus::Interrupted
                            } else {
                                ItemStatus::Completed
                            },
                        });
                    }
                    Some("custom") if msg.get("display").and_then(Value::as_bool) == Some(true) => {
                        let text = content_text(msg.get("content").unwrap_or(&Value::Null));
                        if !text.is_empty() {
                            current(&mut turns)
                                .items
                                .push(notice_item(text, "extensionMessage"));
                        }
                    }
                    _ => {}
                }
            }
            Some("compaction") => {
                current(&mut turns)
                    .items
                    .push(notice_item("Conversation compacted".into(), "compaction"));
            }
            Some("branch_summary") => {
                let summary = entry
                    .get("summary")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                current(&mut turns).items.push(notice_item(
                    format!("Branch summary: {summary}"),
                    "branchSummary",
                ));
            }
            _ => {}
        }
    }
    // Tool calls without a result were cut off.
    for (t, i, _) in open_tools.into_values() {
        if turns[t].items[i].status == ItemStatus::InProgress {
            turns[t].items[i].status = ItemStatus::Interrupted;
        }
    }
    NativeHistory { title, turns }
}

fn notice_item(message: String, code: &str) -> HistoryItem {
    HistoryItem {
        body: ItemBody::Notice {
            level: NoticeLevel::Info,
            message,
            code: Some(code.to_owned()),
        },
        status: ItemStatus::Completed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn entries() -> Vec<Value> {
        vec![
            json!({"type":"session","version":3,"id":"s1","timestamp":"2026-09-27T00:00:00Z","cwd":"/w"}),
            json!({"type":"model_change","id":"a","parentId":null,"provider":"p","modelId":"m"}),
            json!({"type":"message","id":"u1","parentId":"a","message":{"role":"user","content":[{"type":"text","text":"Run echo\nplease"}],"timestamp":1000}}),
            json!({"type":"message","id":"a1","parentId":"u1","message":{"role":"assistant","content":[{"type":"thinking","thinking":"ok"},{"type":"toolCall","id":"c1","name":"bash","arguments":{"command":"echo hi"}}],"stopReason":"toolUse","timestamp":1100}}),
            json!({"type":"message","id":"t1","parentId":"a1","message":{"role":"toolResult","toolCallId":"c1","toolName":"bash","content":[{"type":"text","text":"hi\n"}],"isError":false,"timestamp":1200}}),
            json!({"type":"message","id":"a2","parentId":"t1","message":{"role":"assistant","content":[{"type":"text","text":"done"}],"stopReason":"stop","timestamp":1300}}),
            // abandoned branch from u1
            json!({"type":"message","id":"x1","parentId":"u1","message":{"role":"assistant","content":[{"type":"text","text":"abandoned"}],"stopReason":"stop","timestamp":1150}}),
            // active branch continues from a2
            json!({"type":"compaction","id":"c","parentId":"a2","summary":"s","tokensBefore":10}),
            json!({"type":"message","id":"u2","parentId":"c","message":{"role":"user","content":"second","timestamp":2000}}),
            json!({"type":"message","id":"a3","parentId":"u2","message":{"role":"assistant","content":[{"type":"text","text":"par"}],"stopReason":"aborted","timestamp":2100}}),
            json!({"type":"session_info","id":"n","parentId":"a3","name":"My session"}),
        ]
    }

    #[test]
    fn history_follows_the_active_branch() {
        let h = history_from_entries(&entries(), &AdapterPolicy::default());
        assert_eq!(h.title.as_deref(), Some("My session"));
        // The name is cut to the engine's policy value for harness titles.
        let short = AdapterPolicy {
            harness_title_chars: 2,
            ..AdapterPolicy::default()
        };
        assert_eq!(
            history_from_entries(&entries(), &short).title.as_deref(),
            Some("My")
        );
        assert_eq!(h.turns.len(), 2);
        let t1 = &h.turns[0];
        assert_eq!((t1.started_at, t1.completed_at), (Some(1000), Some(1300)));
        let kinds: Vec<&str> = t1.items.iter().map(|i| i.body.kind_str()).collect();
        assert_eq!(
            kinds,
            vec![
                "userMessage",
                "reasoning",
                "commandExecution",
                "agentMessage",
                "notice"
            ]
        );
        match &t1.items[2] {
            HistoryItem {
                body:
                    ItemBody::CommandExecution {
                        command, output, ..
                    },
                status,
            } => {
                assert_eq!(command, "echo hi");
                assert_eq!(output, "hi\n");
                assert_eq!(*status, ItemStatus::Completed);
            }
            other => panic!("{other:?}"),
        }
        assert!(
            !t1.items
                .iter()
                .any(|i| matches!(&i.body, ItemBody::AgentMessage { text } if text == "abandoned"))
        );
        let t2 = &h.turns[1];
        assert_eq!(t2.items[1].status, ItemStatus::Interrupted);
    }

    #[test]
    fn listing_matches_header_cwd() {
        let agent = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let folder = agent.path().join("sessions").join("--anything--");
        std::fs::create_dir_all(&folder).unwrap();
        let write = |name: &str, id: &str, dir: &Path, ts: i64| {
            let header = json!({"type":"session","version":3,"id":id,"timestamp":"t","cwd":dir.to_string_lossy()});
            let msg = json!({"type":"message","id":"u","parentId":null,"message":{"role":"user","content":"hello there","timestamp":ts}});
            std::fs::write(folder.join(name), format!("{header}\n{msg}\n")).unwrap();
        };
        write("a.jsonl", "id-a", cwd.path(), 5);
        write("b.jsonl", "id-b", other.path(), 9);
        write("c.jsonl", "id-c", cwd.path(), 7);
        let inputs = PathInputs {
            agent_dir_option: Some(agent.path().into()),
            ..Default::default()
        };
        let scan = list_sessions(&inputs, cwd.path(), &AdapterPolicy::default()).unwrap();
        assert!(scan.unreadable.is_empty(), "{:?}", scan.unreadable);
        let list = scan.sessions;
        let ids: Vec<&str> = list.iter().map(|s| s.native_session_id.as_str()).collect();
        assert_eq!(ids, vec!["id-c", "id-a"]);
        assert_eq!(list[0].title.as_deref(), Some("hello there"));
        assert_eq!(
            find_session_file(&inputs, cwd.path(), "id-b").unwrap(),
            Some(folder.join("b.jsonl"))
        );
        assert_eq!(
            find_session_file(&inputs, cwd.path(), "missing").unwrap(),
            None
        );
    }

    /// A session file that cannot be read is skipped and reported with its path; the readable
    /// sessions are still listed, and a torn last line does not make a session unreadable.
    #[test]
    fn unreadable_session_files_are_reported_and_skipped() {
        let agent = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let folder = agent.path().join("sessions").join("--p--");
        std::fs::create_dir_all(&folder).unwrap();
        let header = json!({"type":"session","version":3,"id":"ok","timestamp":"t","cwd":cwd.path().to_string_lossy()});
        let msg = json!({"type":"message","id":"u","parentId":null,"message":{"role":"user","content":"hello","timestamp":5}});
        std::fs::write(
            folder.join("ok.jsonl"),
            format!("{header}\n{msg}\n{{\"type\":\"mess"),
        )
        .unwrap();
        let broken = folder.join("broken.jsonl");
        std::fs::write(&broken, "not json at all\n").unwrap();
        // JSON, but not a pi session: left out without being reported.
        std::fs::write(folder.join("other.jsonl"), "{\"type\":\"note\"}\n").unwrap();
        let inputs = PathInputs {
            agent_dir_option: Some(agent.path().into()),
            ..Default::default()
        };
        let scan = list_sessions(&inputs, cwd.path(), &AdapterPolicy::default()).unwrap();
        let ids: Vec<&str> = scan
            .sessions
            .iter()
            .map(|s| s.native_session_id.as_str())
            .collect();
        assert_eq!(ids, vec!["ok"]);
        assert_eq!(scan.unreadable.len(), 1, "{:?}", scan.unreadable);
        assert_eq!(scan.unreadable[0].location, broken.display().to_string());
        // Found despite the broken file; a missing id is an error that names the broken file.
        assert!(
            find_session_file(&inputs, cwd.path(), "ok")
                .unwrap()
                .is_some()
        );
        let err = find_session_file(&inputs, cwd.path(), "missing").unwrap_err();
        assert!(err.to_string().contains("broken.jsonl"), "{err}");
    }
}
