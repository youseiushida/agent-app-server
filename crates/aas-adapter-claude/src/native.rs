//! Claude Code's own session transcripts (`<config>/projects/<dir>/<sessionId>.jsonl`).
//!
//! Sessions are matched to a working directory by the `cwd` recorded *inside* each
//! transcript; the directory names are never decoded. Turns are grouped by the explicit
//! `promptId` of user entries. Unknown entry types are skipped and counted.
//!
//! Failures are reported, never turned into an empty result: a projects folder that cannot
//! be read fails the listing; a project folder or transcript that cannot be read is skipped
//! and returned as unreadable (with its path); a line that is not JSON (e.g. the half-written
//! last line of a session Claude Code is writing right now) is skipped and logged with its
//! path and line number.

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use aas_harness::protocol::{ItemBody, ItemStatus, NoticeLevel, UserMessageDelivery};
use aas_harness::{
    AdapterError, HistoryItem, HistoryTurn, Millis, NativeHistory, NativeSessionScan,
    NativeSessionSummary, UnreadableNativeSession,
};
use serde_json::Value;

use crate::mapping::{self, TaskList, ToolClass, ToolResult};
use crate::time::parse_rfc3339_millis;

/// Most characters of a title made from a prompt (Claude Code's own rule for untitled
/// sessions); a longer first line is cut and ends with `…`.
const PROMPT_TITLE_MAX_CHARS: usize = 80;

/// `CLAUDE_CONFIG_DIR`, or `~/.claude`.
pub fn claude_config_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR").filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    dirs::home_dir().map(|h| h.join(".claude"))
}

/// Normalized form of a path for comparison (separators unified, no trailing separator,
/// case-insensitive on Windows).
pub fn normalize_path(p: &str) -> String {
    let trimmed = p.trim_end_matches(['/', '\\']);
    if cfg!(windows) {
        trimmed.replace('/', "\\").to_lowercase()
    } else {
        trimmed.to_owned()
    }
}

fn unreadable(path: &Path, error: impl std::fmt::Display) -> UnreadableNativeSession {
    UnreadableNativeSession {
        location: path.display().to_string(),
        error: error.to_string(),
    }
}

/// Every transcript under `projects_dir` (`<projects>/<dir>/<id>.jsonl`), and the folders and
/// entries that could not be read. A missing `projects_dir` holds no transcripts (Claude Code
/// has not been used yet); any other failure to read it is an error.
fn transcript_files(
    projects_dir: &Path,
) -> Result<(Vec<PathBuf>, Vec<UnreadableNativeSession>), AdapterError> {
    let mut files = Vec::new();
    let mut skipped = Vec::new();
    let dirs = match std::fs::read_dir(projects_dir) {
        Ok(dirs) => dirs,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((files, skipped)),
        Err(e) => {
            return Err(AdapterError::Other(format!(
                "cannot read the Claude Code projects folder {}: {e}",
                projects_dir.display()
            )));
        }
    };
    for dir in dirs {
        let dir = match dir {
            Ok(dir) => dir,
            Err(e) => {
                skipped.push(unreadable(projects_dir, e));
                continue;
            }
        };
        let path = dir.path();
        if !path.is_dir() {
            continue;
        }
        let entries = match std::fs::read_dir(&path) {
            Ok(entries) => entries,
            Err(e) => {
                skipped.push(unreadable(&path, e));
                continue;
            }
        };
        for entry in entries {
            match entry {
                Ok(entry) => {
                    let p = entry.path();
                    if p.extension().is_some_and(|e| e == "jsonl") && p.is_file() {
                        files.push(p);
                    }
                }
                Err(e) => skipped.push(unreadable(&path, e)),
            }
        }
    }
    Ok((files, skipped))
}

/// The JSON entries of a transcript, in order. Lines that are not JSON are skipped and logged
/// (Claude Code may be writing the last line right now); a read error fails the whole file.
fn lines(path: &Path) -> std::io::Result<Vec<Value>> {
    let file = std::fs::File::open(path)?;
    let mut out = Vec::new();
    for (index, line) in BufReader::new(file).lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(&line) {
            Ok(v) => out.push(v),
            Err(e) => tracing::warn!(
                path = %path.display(),
                line = index + 1,
                error = %e,
                "skipped a transcript line that is not JSON"
            ),
        }
    }
    Ok(out)
}

fn str_of<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

/// The first `cwd` recorded in a set of transcript entries.
fn recorded_cwd(entries: &[Value]) -> Option<String> {
    entries
        .iter()
        .find_map(|v| str_of(v, "cwd").map(str::to_owned))
}

/// Text of a user prompt (`message.content` string, or its text blocks).
fn prompt_text(message: &Value) -> Option<String> {
    match message.get("content")? {
        Value::String(s) => Some(s.clone()),
        Value::Array(blocks) => {
            if blocks
                .iter()
                .any(|b| str_of(b, "type") == Some("tool_result"))
            {
                return None;
            }
            let texts: Vec<&str> = blocks
                .iter()
                .filter(|b| str_of(b, "type") == Some("text"))
                .filter_map(|b| str_of(b, "text"))
                .collect();
            let has_image = blocks.iter().any(|b| str_of(b, "type") == Some("image"));
            if texts.is_empty() && !has_image {
                return None;
            }
            Some(texts.join("\n"))
        }
        _ => None,
    }
}

fn is_prompt_entry(v: &Value) -> bool {
    str_of(v, "type") == Some("user")
        && v.get("isSidechain").and_then(Value::as_bool) != Some(true)
        && v.get("isMeta").and_then(Value::as_bool) != Some(true)
        && v.get("isCompactSummary").and_then(Value::as_bool) != Some(true)
        && v.get("message").and_then(prompt_text).is_some()
}

/// First line of a prompt, at most 80 characters (the rule used for untitled sessions).
pub fn title_from_prompt(prompt: &str) -> String {
    let first = prompt
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    let mut out: String = first.chars().take(PROMPT_TITLE_MAX_CHARS).collect();
    if first.chars().count() > PROMPT_TITLE_MAX_CHARS {
        out.push('…');
    }
    out
}

/// Sessions whose transcript records `cwd`, newest first, and the transcripts (or folders)
/// that could not be read.
pub fn list_sessions(projects_dir: &Path, cwd: &Path) -> Result<NativeSessionScan, AdapterError> {
    let wanted = normalize_path(&cwd.to_string_lossy());
    let (files, mut skipped) = transcript_files(projects_dir)?;
    let mut out = Vec::new();
    for file in files {
        let entries = match lines(&file) {
            Ok(entries) => entries,
            Err(e) => {
                skipped.push(unreadable(&file, e));
                continue;
            }
        };
        let Some(recorded) = recorded_cwd(&entries) else {
            continue;
        };
        if normalize_path(&recorded) != wanted {
            continue;
        }
        let mut session_id: Option<String> = None;
        let mut custom_title = None;
        let mut ai_title = None;
        let mut summary = None;
        let mut first_prompt = None;
        let mut updated_at: Option<Millis> = None;
        for v in entries {
            if session_id.is_none() {
                session_id = str_of(&v, "sessionId").map(str::to_owned);
            }
            match str_of(&v, "type") {
                Some("custom-title") => custom_title = str_of(&v, "customTitle").map(str::to_owned),
                Some("ai-title") => ai_title = str_of(&v, "aiTitle").map(str::to_owned),
                Some("summary") => summary = str_of(&v, "summary").map(str::to_owned),
                _ => {}
            }
            if first_prompt.is_none() && is_prompt_entry(&v) {
                first_prompt = v.get("message").and_then(prompt_text);
            }
            if let Some(ts) = str_of(&v, "timestamp").and_then(parse_rfc3339_millis) {
                updated_at = Some(updated_at.map_or(ts, |u: Millis| u.max(ts)));
            }
        }
        if first_prompt.is_none() {
            // No user prompt: an empty or probe-only session.
            continue;
        }
        let id = session_id
            .or_else(|| file.file_stem().map(|s| s.to_string_lossy().into_owned()))
            .unwrap_or_default();
        let title = custom_title
            .or(ai_title)
            .or(summary)
            .or_else(|| first_prompt.as_deref().map(title_from_prompt));
        out.push(NativeSessionSummary {
            native_session_id: id,
            title,
            updated_at,
            cwd: Some(recorded),
        });
    }
    out.sort_by(|a, b| {
        b.updated_at
            .cmp(&a.updated_at)
            .then_with(|| a.native_session_id.cmp(&b.native_session_id))
    });
    Ok(NativeSessionScan {
        sessions: out,
        unreadable: skipped,
    })
}

/// Finds the transcript of `session_id` recorded for `cwd`. `Ok(None)` when there is none; a
/// transcript of that id that cannot be read is an error (it may be the one asked for).
pub fn find_transcript(
    projects_dir: &Path,
    cwd: &Path,
    session_id: &str,
) -> Result<Option<PathBuf>, AdapterError> {
    let wanted = normalize_path(&cwd.to_string_lossy());
    let (files, skipped) = transcript_files(projects_dir)?;
    for skipped in &skipped {
        tracing::warn!(location = %skipped.location, error = %skipped.error, "skipped an unreadable Claude Code project folder while looking for a session");
    }
    for file in files {
        if !file
            .file_stem()
            .is_some_and(|s| s.to_string_lossy() == session_id)
        {
            continue;
        }
        let entries = lines(&file)
            .map_err(|e| AdapterError::Other(format!("cannot read {}: {e}", file.display())))?;
        if recorded_cwd(&entries).is_some_and(|c| normalize_path(&c) == wanted) {
            return Ok(Some(file));
        }
    }
    Ok(None)
}

/// Parses a transcript into turns and items.
pub fn read_history(path: &Path) -> Result<NativeHistory, AdapterError> {
    let entries = lines(path)
        .map_err(|e| AdapterError::Other(format!("cannot read {}: {e}", path.display())))?;
    let mut history = NativeHistory::default();
    let mut custom_title = None;
    let mut ai_title = None;
    let mut summary = None;
    let mut first_prompt: Option<String> = None;
    let mut current_prompt: Option<String> = None;
    let mut turn: Option<HistoryTurn> = None;
    // tool_use id → (index of the item in the current turn or None for plan tools, name, input)
    let mut tools: HashMap<String, (Option<usize>, String, Value)> = HashMap::new();
    let mut tasks = TaskList::default();
    let mut plan_index: Option<usize> = None;
    let mut skipped: HashMap<String, usize> = HashMap::new();

    let flush = |turn: &mut Option<HistoryTurn>, history: &mut NativeHistory| {
        if let Some(t) = turn.take() {
            history.turns.push(t);
        }
    };

    for v in entries {
        let kind = str_of(&v, "type").unwrap_or("").to_owned();
        if v.get("isSidechain").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        let ts = str_of(&v, "timestamp").and_then(parse_rfc3339_millis);
        match kind.as_str() {
            "custom-title" => custom_title = str_of(&v, "customTitle").map(str::to_owned),
            "ai-title" => ai_title = str_of(&v, "aiTitle").map(str::to_owned),
            "summary" => summary = str_of(&v, "summary").map(str::to_owned),
            "user" => {
                if v.get("isMeta").and_then(Value::as_bool) == Some(true)
                    || v.get("isCompactSummary").and_then(Value::as_bool) == Some(true)
                {
                    continue;
                }
                let Some(message) = v.get("message") else {
                    continue;
                };
                let prompt_id = str_of(&v, "promptId").map(str::to_owned);
                if let Some(text) = prompt_text(message) {
                    let new_turn =
                        turn.is_none() || prompt_id.is_none() || prompt_id != current_prompt;
                    if new_turn {
                        flush(&mut turn, &mut history);
                        tools.clear();
                        plan_index = None;
                        current_prompt = prompt_id;
                        if first_prompt.is_none() {
                            first_prompt = Some(text.clone());
                        }
                        turn = Some(HistoryTurn {
                            started_at: ts,
                            completed_at: ts,
                            items: Vec::new(),
                        });
                        let t = turn.as_mut().expect("turn just created");
                        t.items.push(HistoryItem {
                            body: ItemBody::UserMessage {
                                text,
                                attachments: Vec::new(),
                                mentions: Vec::new(),
                                delivery: UserMessageDelivery::Normal,
                            },
                            status: ItemStatus::Completed,
                        });
                    }
                    // A text entry of the same prompt (e.g. the CLI's interrupt marker) is not a
                    // new message.
                    continue;
                }
                let Some(t) = turn.as_mut() else { continue };
                if let Some(ts) = ts {
                    t.completed_at = Some(ts);
                }
                let blocks = message
                    .get("content")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                let results: Vec<&Value> = blocks
                    .iter()
                    .filter(|b| str_of(b, "type") == Some("tool_result"))
                    .collect();
                let structured = if results.len() == 1 {
                    v.get("toolUseResult").cloned()
                } else {
                    None
                };
                for block in results {
                    let Some(id) = str_of(block, "tool_use_id") else {
                        continue;
                    };
                    let Some((index, name, input)) = tools.remove(id) else {
                        continue;
                    };
                    let result = ToolResult {
                        text: mapping::tool_result_text(
                            block.get("content").unwrap_or(&Value::Null),
                        ),
                        is_error: block.get("is_error").and_then(Value::as_bool) == Some(true),
                        structured: structured.clone(),
                        denied_by_user: v.get("toolDenialKind").is_some(),
                    };
                    if mapping::classify_tool(&name) == ToolClass::Plan {
                        if tasks.apply(&name, &input, &result) {
                            let body = ItemBody::Plan {
                                entries: tasks.entries(),
                            };
                            match plan_index {
                                Some(i) => t.items[i].body = body,
                                None => {
                                    plan_index = Some(t.items.len());
                                    t.items.push(HistoryItem {
                                        body,
                                        status: ItemStatus::Completed,
                                    });
                                }
                            }
                        }
                        continue;
                    }
                    if let Some(i) = index {
                        let started = t.items[i].body.clone();
                        let (body, status) =
                            mapping::tool_completed(&name, &input, &started, &result);
                        t.items[i] = HistoryItem { body, status };
                    }
                }
            }
            "assistant" => {
                let Some(t) = turn.as_mut() else { continue };
                if let Some(ts) = ts {
                    t.completed_at = Some(ts);
                }
                let blocks = v
                    .pointer("/message/content")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                for block in blocks {
                    match str_of(&block, "type") {
                        Some("text") => {
                            let text = str_of(&block, "text").unwrap_or("").to_owned();
                            if !text.is_empty() {
                                t.items.push(HistoryItem {
                                    body: ItemBody::AgentMessage { text },
                                    status: ItemStatus::Completed,
                                });
                            }
                        }
                        Some("thinking") => {
                            let text = str_of(&block, "thinking").unwrap_or("").to_owned();
                            if !text.is_empty() {
                                t.items.push(HistoryItem {
                                    body: ItemBody::Reasoning { text },
                                    status: ItemStatus::Completed,
                                });
                            }
                        }
                        Some("tool_use") => {
                            let Some(id) = str_of(&block, "id").map(str::to_owned) else {
                                continue;
                            };
                            let name = str_of(&block, "name").unwrap_or("").to_owned();
                            let input = block.get("input").cloned().unwrap_or(Value::Null);
                            let index = mapping::tool_started_body(&name, &input).map(|body| {
                                // A tool without a recorded result stays interrupted.
                                t.items.push(HistoryItem {
                                    body,
                                    status: ItemStatus::Interrupted,
                                });
                                t.items.len() - 1
                            });
                            tools.insert(id, (index, name, input));
                        }
                        _ => {}
                    }
                }
            }
            "system" => {
                if str_of(&v, "subtype") == Some("compact_boundary")
                    && let Some(t) = turn.as_mut()
                {
                    t.items.push(HistoryItem {
                        body: ItemBody::Notice {
                            level: NoticeLevel::Info,
                            message: "Conversation compacted".into(),
                            code: Some("compacted".into()),
                        },
                        status: ItemStatus::Completed,
                    });
                }
            }
            "attachment"
            | "queue-operation"
            | "atis-latch"
            | "last-prompt"
            | "cost-state"
            | "file-history-snapshot"
            | "permission-mode"
            | "mode"
            | "bridge-session" => {}
            other => *skipped.entry(other.to_owned()).or_default() += 1,
        }
    }
    flush(&mut turn, &mut history);
    if !skipped.is_empty() {
        tracing::debug!(path = %path.display(), ?skipped, "skipped unknown transcript entry types");
    }
    history.title = custom_title
        .or(ai_title)
        .or(summary)
        .or_else(|| first_prompt.as_deref().map(title_from_prompt));
    Ok(history)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use std::io::Write;

    fn write_transcript(dir: &Path, sub: &str, id: &str, lines: &[Value]) -> PathBuf {
        let d = dir.join(sub);
        std::fs::create_dir_all(&d).unwrap();
        let path = d.join(format!("{id}.jsonl"));
        let mut f = std::fs::File::create(&path).unwrap();
        for l in lines {
            writeln!(f, "{l}").unwrap();
        }
        path
    }

    fn sample(cwd: &str, id: &str) -> Vec<Value> {
        use serde_json::json;
        vec![
            json!({"type": "queue-operation", "operation": "enqueue", "sessionId": id}),
            json!({"type": "user", "promptId": "p1", "message": {"role": "user", "content": "Create note.txt\nplease"},
                   "cwd": cwd, "sessionId": id, "timestamp": "2026-09-27T03:51:33.793Z"}),
            json!({"type": "attachment", "attachment": {"type": "date"}, "cwd": cwd, "sessionId": id}),
            json!({"type": "assistant", "message": {"id": "m1", "content": [{"type": "thinking", "thinking": ""}]},
                   "timestamp": "2026-09-27T03:51:34.000Z"}),
            json!({"type": "assistant", "message": {"id": "m1", "content": [{"type": "tool_use", "id": "t1", "name": "Write",
                   "input": {"file_path": "C:\\w\\note.txt", "content": "hello"}}]}, "timestamp": "2026-09-27T03:51:35.000Z"}),
            json!({"type": "user", "promptId": "p1", "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1",
                   "content": "File created"}]}, "toolUseResult": {"type": "create", "filePath": "C:\\w\\note.txt", "content": "hello",
                   "structuredPatch": []}, "timestamp": "2026-09-27T03:51:36.000Z"}),
            json!({"type": "assistant", "message": {"id": "m2", "content": [{"type": "text", "text": "done"}]},
                   "timestamp": "2026-09-27T03:51:37.000Z"}),
            json!({"type": "user", "promptId": "p2", "message": {"role": "user", "content": [{"type": "text", "text": "Count"}]},
                   "timestamp": "2026-09-27T03:52:00.000Z"}),
            json!({"type": "user", "promptId": "p2", "message": {"role": "user", "content": [{"type": "text", "text": "[Request interrupted by user]"}]},
                   "timestamp": "2026-09-27T03:52:01.000Z"}),
            json!({"type": "user", "isMeta": true, "promptId": "p3", "message": {"role": "user", "content": "meta"}}),
            json!({"type": "ai-title", "aiTitle": "Note creation", "sessionId": id}),
            json!({"type": "brand-new-entry", "x": 1}),
        ]
    }

    #[test]
    fn lists_sessions_by_recorded_cwd() {
        let dir = tempfile::tempdir().unwrap();
        write_transcript(dir.path(), "a", "s1", &sample("C:\\Work\\Proj", "s1"));
        write_transcript(dir.path(), "b", "s2", &sample("C:\\Other", "s2"));
        // A transcript without any prompt is not listed.
        write_transcript(
            dir.path(),
            "a",
            "s3",
            &[serde_json::json!({"type": "attachment", "cwd": "C:\\Work\\Proj"})],
        );
        let cwd = if cfg!(windows) {
            "c:/work/proj/"
        } else {
            "C:\\Work\\Proj"
        };
        let scan = list_sessions(dir.path(), Path::new(cwd)).unwrap();
        assert!(scan.unreadable.is_empty(), "{:?}", scan.unreadable);
        let list = scan.sessions;
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].native_session_id, "s1");
        assert_eq!(list[0].title.as_deref(), Some("Note creation"));
        assert_eq!(
            list[0].updated_at,
            parse_rfc3339_millis("2026-09-27T03:52:01.000Z")
        );
        assert!(
            find_transcript(dir.path(), Path::new(cwd), "s1")
                .unwrap()
                .is_some()
        );
        assert!(
            find_transcript(dir.path(), Path::new(cwd), "s2")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn a_missing_projects_folder_holds_no_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let scan = list_sessions(&dir.path().join("never-created"), Path::new("C:\\p")).unwrap();
        assert_eq!(scan, NativeSessionScan::default());
    }

    #[test]
    fn a_projects_path_that_is_not_a_folder_fails_the_listing() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("projects");
        std::fs::write(&file, b"not a folder").unwrap();
        assert!(matches!(
            list_sessions(&file, Path::new("C:\\p")),
            Err(AdapterError::Other(_))
        ));
    }

    /// A transcript that cannot be read is skipped and reported with its path; the readable
    /// sessions are still listed. Lines that are not JSON do not make a session unreadable.
    #[test]
    fn unreadable_transcripts_are_reported_and_skipped() {
        let dir = tempfile::tempdir().unwrap();
        write_transcript(dir.path(), "a", "s1", &sample("C:\\Work\\Proj", "s1"));
        // A torn last line, as while Claude Code is writing.
        let torn = write_transcript(dir.path(), "a", "s2", &sample("C:\\Work\\Proj", "s2"));
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&torn)
            .unwrap();
        write!(f, "{{\"type\": \"assistant\", \"mess").unwrap();
        drop(f);
        // Not UTF-8: reading the file fails.
        let broken = dir.path().join("a").join("s3.jsonl");
        std::fs::write(&broken, [0xff, 0xfe, b'\n']).unwrap();
        let scan = list_sessions(dir.path(), Path::new("C:\\Work\\Proj")).unwrap();
        let ids: Vec<&str> = scan
            .sessions
            .iter()
            .map(|s| s.native_session_id.as_str())
            .collect();
        assert_eq!(ids.len(), 2, "{ids:?}");
        assert!(ids.contains(&"s1") && ids.contains(&"s2"), "{ids:?}");
        assert_eq!(scan.unreadable.len(), 1, "{:?}", scan.unreadable);
        assert_eq!(scan.unreadable[0].location, broken.display().to_string());
        // The broken transcript of the id asked for is an error, not "no such session".
        assert!(find_transcript(dir.path(), Path::new("C:\\Work\\Proj"), "s3").is_err());
    }

    #[test]
    fn reads_history_grouped_by_prompt_id() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_transcript(dir.path(), "a", "s1", &sample("C:\\Work\\Proj", "s1"));
        let h = read_history(&path).unwrap();
        assert_eq!(h.title.as_deref(), Some("Note creation"));
        assert_eq!(h.turns.len(), 2);
        let t0 = &h.turns[0];
        assert_eq!(t0.items.len(), 3);
        assert!(
            matches!(&t0.items[0].body, ItemBody::UserMessage { text, .. } if text == "Create note.txt\nplease")
        );
        match &t0.items[1].body {
            ItemBody::FileChange { changes } => {
                assert_eq!(changes[0].kind, aas_harness::protocol::FileChangeKind::Add)
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(t0.items[1].status, ItemStatus::Completed);
        assert!(matches!(&t0.items[2].body, ItemBody::AgentMessage { text } if text == "done"));
        assert_eq!(
            t0.started_at,
            parse_rfc3339_millis("2026-09-27T03:51:33.793Z")
        );
        assert_eq!(
            t0.completed_at,
            parse_rfc3339_millis("2026-09-27T03:51:37.000Z")
        );
        // The interrupt marker belongs to prompt p2 and does not open a turn.
        assert_eq!(h.turns[1].items.len(), 1);
    }

    #[test]
    fn titles_from_prompts() {
        assert_eq!(
            title_from_prompt("\n  hello world  \nsecond"),
            "hello world"
        );
        let long = "x".repeat(100);
        assert_eq!(title_from_prompt(&long).chars().count(), 81);
    }
}
