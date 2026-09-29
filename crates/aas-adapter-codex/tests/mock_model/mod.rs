//! A scripted model for live tests of the real `codex app-server`: a local Responses API
//! endpoint (HTTP/1.1, streamed SSE) that answers each sampling request from a fixed script,
//! the way Codex's own integration tests do (`codex-rs/core/tests/common/responses.rs`).
//!
//! Everything else is the installed CLI: app-server, its tools, unified exec, sub-agent
//! threads, approvals and interrupts. The model's choices (which tool, with which arguments)
//! are the script's, so the test is deterministic and spends no tokens.
//!
//! A request's *role* is the last `ROLE=<name>#` token in a message item of its input (tool
//! call arguments and outputs are not messages, so a parent's `spawn_agent` arguments do not
//! count), or the role of one of Codex's own fixed texts a user message starts with (see
//! [`FIXED_ROLES`]); its *step* is the number of the script's own calls of that role (call ids
//! `mock_<role>_<n>`) after that message that already have an output.

// Each test crate that includes this module uses a different subset of it.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

/// Runs well past its turn (ten minutes); stopped by the tests.
pub const LONG: &str =
    r#"for ($i=1; $i -le 600; $i++) { Write-Output "TICK_L $i"; Start-Sleep -Seconds 1 }"#;
/// Outlives its turn by several seconds, then ends by itself.
pub const SHORT: &str = r#"for ($i=1; $i -le 25; $i++) { Write-Output "TICK_S $i"; Start-Sleep -Seconds 1 }; Write-Output SHORT_DONE"#;
/// The sub-agent's command: runs long after the sub-agent is stopped.
pub const WORKER_CMD: &str = "Start-Sleep -Seconds 300; Write-Output WORKER_OK";

/// The background scenarios: `TERM` starts [`LONG`] and [`SHORT`] and ends its turn; `SPAWN`
/// spawns the v2 sub-agent `worker` and ends its turn; `worker` runs [`WORKER_CMD`] and waits
/// for it.
pub fn background_script(role: &str, step: usize) -> Reply {
    let exec = |cmd: &str, yield_ms: u64| Reply::Call {
        name: "exec_command",
        namespace: None,
        arguments: json!({ "cmd": cmd, "yield_time_ms": yield_ms }),
    };
    match (role, step) {
        ("TERM", 0) => exec(LONG, 1000),
        ("TERM", 1) => exec(SHORT, 1000),
        ("TERM", _) => Reply::Message("STARTED"),
        ("SPAWN", 0) => Reply::Call {
            name: "spawn_agent",
            namespace: Some("collaboration"),
            arguments: json!({
                "task_name": "worker",
                "message": "ROLE=worker# Run the long command, then reply WORKER_DONE.",
            }),
        },
        ("SPAWN", _) => Reply::Message("SPAWNED"),
        // The sub-agent's turn waits for its command until it is stopped.
        ("worker", 0) => exec(WORKER_CMD, 30000),
        ("worker", _) => Reply::Message("WORKER_DONE"),
        _ => Reply::Message("MOCK_UNHANDLED"),
    }
}

/// A `CODEX_HOME` under `dir` whose only configuration is the scripted model at `base_url`
/// (and v2 sub-agents): the user's configuration, credentials and sessions stay untouched.
pub fn codex_home(dir: &Path, base_url: &str) -> PathBuf {
    let home = dir.join("codex-home");
    std::fs::create_dir_all(&home).expect("codex home");
    let config = [
        r#"model = "mock-model""#.to_owned(),
        r#"model_provider = "mock""#.to_owned(),
        String::new(),
        "[model_providers.mock]".to_owned(),
        r#"name = "mock""#.to_owned(),
        format!(r#"base_url = "{base_url}""#),
        r#"wire_api = "responses""#.to_owned(),
        "request_max_retries = 0".to_owned(),
        "stream_max_retries = 0".to_owned(),
        String::new(),
        "[features]".to_owned(),
        "multi_agent_v2 = true".to_owned(),
    ]
    .join("\n");
    std::fs::write(home.join("config.toml"), config).expect("codex config");
    home
}

/// The command of the `GOALSTOP` continuation: it outlasts the moment the test interrupts.
pub const GOAL_WAIT: &str = "Start-Sleep -Seconds 60; Write-Output GOAL_WAITED";

/// Roles of user messages that are Codex's own fixed texts (they carry no role token).
pub const FIXED_ROLES: [(&str, &str); 3] = [
    ("Implement the plan.", "IMPL"),
    ("A previous agent produced the plan below", "IMPL_FRESH"),
    ("Generate a file named AGENTS.md", "INIT"),
];

/// The plan the `PLAN` role proposes (inside the reply's `<proposed_plan>` block).
pub const PLAN_BODY: &str = "# Rename greeting

1. Change `hello.txt` so it says `Hello, world`.
2. Verify the file content.";
const PLAN_REPLY: &str = "I read the task and prepared a plan.

<proposed_plan>
# Rename greeting

1. Change `hello.txt` so it says `Hello, world`.
2. Verify the file content.
</proposed_plan>

Switch to Default mode to implement it.";
/// The review the `REVIEW` role returns (Codex's review output schema).
const REVIEW_REPLY: &str = r#"{"findings":[{"title":"[P2] Greeting is missing a comma","body":"`hello.txt` says `Hello world`; the requested text is `Hello, world`.","confidence_score":0.8,"priority":2,"code_location":{"absolute_file_path":"hello.txt","line_range":{"start":1,"end":1}}}],"overall_correctness":"patch is incorrect","overall_explanation":"One wording problem in hello.txt.","overall_confidence_score":0.7}"#;

/// The features scenarios: plan mode (`PLAN`, then Codex's implement text), `/init`, the
/// inline review (`REVIEW`), a goal the model completes with `update_goal` (`GOAL`), a goal
/// whose continuation waits until it is interrupted (`GOALSTOP`), and plain answers (`FORKED`,
/// `BEFORE`).
pub fn features_script(role: &str, step: usize) -> Reply {
    match (role, step) {
        ("PLAN", _) => Reply::Message(PLAN_REPLY),
        ("IMPL", _) => Reply::Message("IMPLEMENTED"),
        ("IMPL_FRESH", _) => Reply::Message("IMPLEMENTED_FRESH"),
        ("INIT", _) => Reply::Message("INIT_DONE"),
        ("REVIEW", _) => Reply::Message(REVIEW_REPLY),
        ("GOAL", 0) => Reply::Call {
            name: "update_goal",
            namespace: None,
            arguments: json!({ "status": "complete" }),
        },
        ("GOAL", _) => Reply::Message("GOAL_DONE"),
        // A goal whose continuation waits on a long command until it is interrupted.
        ("GOALSTOP", 0) => Reply::Call {
            name: "exec_command",
            namespace: None,
            arguments: json!({ "cmd": GOAL_WAIT, "yield_time_ms": 30000 }),
        },
        ("GOALSTOP", _) => Reply::Message("GOALSTOP_DONE"),
        ("FORKED", _) => Reply::Message("FORKED"),
        ("BEFORE", _) => Reply::Message("BEFORE"),
        _ => Reply::Message("MOCK_UNHANDLED"),
    }
}

/// What the model answers.
#[derive(Debug, Clone)]
pub enum Reply {
    /// A function call (`namespace` for namespaced tools such as `collaboration`).
    Call {
        name: &'static str,
        namespace: Option<&'static str>,
        arguments: Value,
    },
    /// A final assistant message.
    Message(&'static str),
}

/// The script: role and step (the call outputs already seen) → reply.
pub type Script = fn(role: &str, step: usize) -> Reply;

/// One sampling request the model answered.
#[derive(Debug, Clone)]
pub struct Sampled {
    pub role: Option<String>,
    pub step: usize,
    pub reply: String,
    /// The request's `service_tier` (`None`: not sent).
    pub service_tier: Option<String>,
    /// The request's `reasoning.effort`.
    pub effort: Option<String>,
    /// The collaboration mode of the last developer message that states one: `plan`,
    /// `default`, or `none` for Codex's empty block (`None`: no block at all).
    pub mode: Option<String>,
    /// The text of the input's last user message.
    pub user_text: String,
}

pub struct MockModel {
    pub base_url: String,
    pub sampled: Arc<Mutex<Vec<Sampled>>>,
    pub errors: Arc<Mutex<Vec<String>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for MockModel {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl MockModel {
    pub async fn start(script: Script) -> MockModel {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("local addr").port();
        let sampled = Arc::new(Mutex::new(Vec::new()));
        let errors = Arc::new(Mutex::new(Vec::new()));
        let (s, e) = (sampled.clone(), errors.clone());
        let task = tokio::spawn(async move {
            let counter = Arc::new(Mutex::new(0usize));
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let (s, e, counter) = (s.clone(), e.clone(), counter.clone());
                tokio::spawn(async move {
                    if let Err(err) = serve(stream, script, &s, &counter).await {
                        e.lock().push(err);
                    }
                });
            }
        });
        MockModel {
            base_url: format!("http://127.0.0.1:{port}/v1"),
            sampled,
            errors,
            task,
        }
    }
}

async fn serve(
    stream: TcpStream,
    script: Script,
    sampled: &Mutex<Vec<Sampled>>,
    counter: &Mutex<usize>,
) -> Result<(), String> {
    let mut reader = BufReader::new(stream);
    let mut request_line = String::new();
    reader
        .read_line(&mut request_line)
        .await
        .map_err(|e| e.to_string())?;
    if request_line.is_empty() {
        return Ok(());
    }
    let mut length = None;
    let mut chunked = false;
    let mut encoded = None;
    loop {
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .await
            .map_err(|e| e.to_string())?;
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            let (name, value) = (name.trim().to_ascii_lowercase(), value.trim());
            match name.as_str() {
                "content-length" => length = value.parse::<usize>().ok(),
                "transfer-encoding" => chunked = value.eq_ignore_ascii_case("chunked"),
                "content-encoding" if !value.eq_ignore_ascii_case("identity") => {
                    encoded = Some(value.to_owned())
                }
                _ => {}
            }
        }
    }
    let mut body = Vec::new();
    if chunked {
        loop {
            let mut size = String::new();
            reader
                .read_line(&mut size)
                .await
                .map_err(|e| e.to_string())?;
            let size = usize::from_str_radix(size.trim().split(';').next().unwrap_or(""), 16)
                .map_err(|e| format!("bad chunk size: {e}"))?;
            let mut chunk = vec![0; size + 2];
            reader
                .read_exact(&mut chunk)
                .await
                .map_err(|e| e.to_string())?;
            if size == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..size]);
        }
    } else if let Some(length) = length {
        body.resize(length, 0);
        reader
            .read_exact(&mut body)
            .await
            .map_err(|e| e.to_string())?;
    }
    let mut stream = reader.into_inner();
    let path = request_line.split_whitespace().nth(1).unwrap_or("");
    if !(request_line.starts_with("POST ") && path.ends_with("/responses")) {
        let text = r#"{"error":{"message":"mock: not found"}}"#;
        return respond(&mut stream, "404 Not Found", "application/json", text).await;
    }
    if let Some(encoding) = encoded {
        return Err(format!("request body encoded as {encoding}"));
    }
    let request: Value = serde_json::from_slice(&body).map_err(|e| format!("request body: {e}"))?;
    let input = request["input"].as_array().cloned().unwrap_or_default();
    let service_tier = request["service_tier"].as_str().map(str::to_owned);
    let effort = request["reasoning"]["effort"].as_str().map(str::to_owned);
    let mode = collaboration_mode(&input);
    let user_text = input
        .iter()
        .rev()
        .find(|i| i["type"] == "message" && i["role"] == "user")
        .map(text_of)
        .unwrap_or_default();
    let (role, at) = role_of(&input);
    let step = match &role {
        Some(role) => calls_done(&input, role, at),
        None => 0,
    };
    let reply = match &role {
        Some(role) => script(role, step),
        None => Reply::Message("MOCK_NO_ROLE"),
    };
    let n = {
        let mut counter = counter.lock();
        *counter += 1;
        *counter
    };
    let (events, described) = sse(&reply, role.as_deref().unwrap_or("none"), step, n);
    sampled.lock().push(Sampled {
        role: role.clone(),
        step,
        reply: described,
        service_tier,
        effort,
        mode,
        user_text,
    });
    respond(&mut stream, "200 OK", "text/event-stream", &events).await
}

async fn respond(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    body: &str,
) -> Result<(), String> {
    let head = format!(
        "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\ncache-control: no-cache\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    );
    stream
        .write_all(head.as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    stream
        .write_all(body.as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    stream.shutdown().await.map_err(|e| e.to_string())
}

/// Text of a message item (plain, input text, or the payload of an inter-agent message).
fn text_of(item: &Value) -> String {
    match &item["content"] {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| {
                ["text", "input_text", "encrypted_content"]
                    .iter()
                    .find_map(|k| p[*k].as_str())
            })
            .collect(),
        _ => String::new(),
    }
}

/// The collaboration mode the input's developer messages state last (see
/// [`Sampled::mode`]).
fn collaboration_mode(input: &[Value]) -> Option<String> {
    input
        .iter()
        .rev()
        .filter(|i| i["type"] == "message" && i["role"] == "developer")
        .find_map(|i| {
            let text = text_of(i);
            // The block opens the message; its text may quote the tag itself.
            let at = text.find("<collaboration_mode>")?;
            let block = &text[at + "<collaboration_mode>".len()..];
            Some(if block.starts_with("</collaboration_mode>") {
                "none".to_owned()
            } else if block.contains("# Plan Mode") {
                "plan".to_owned()
            } else if block.contains("Collaboration Mode: Default") {
                "default".to_owned()
            } else {
                format!("other: {}", block.chars().take(160).collect::<String>())
            })
        })
}

/// The last role token of the input's messages and the index of its message.
fn role_of(input: &[Value]) -> (Option<String>, usize) {
    let mut found = (None, 0);
    for (i, item) in input.iter().enumerate() {
        let kind = item["type"].as_str().unwrap_or_default();
        if kind != "message" && kind != "agent_message" {
            continue;
        }
        let text = text_of(item);
        if item["role"] == "user"
            && let Some((_, role)) = FIXED_ROLES.iter().find(|(t, _)| text.starts_with(t))
        {
            found = (Some((*role).to_owned()), i);
            continue;
        }
        let mut rest = text.as_str();
        while let Some(at) = rest.find("ROLE=") {
            let after = &rest[at + 5..];
            if let Some(end) = after.find('#') {
                let name = &after[..end];
                if !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                    found = (Some(name.to_owned()), i);
                }
            }
            rest = &rest[at + 5..];
        }
    }
    found
}

/// How many of the script's calls of `role` after message `at` have an output.
fn calls_done(input: &[Value], role: &str, at: usize) -> usize {
    let prefix = format!("mock_{role}_");
    input[at..]
        .iter()
        .filter(|i| i["type"] == "function_call")
        .filter_map(|i| i["call_id"].as_str())
        .filter(|id| id.starts_with(&prefix))
        .filter(|call| {
            input[at..].iter().any(|i| {
                i["type"] == "function_call_output" && i["call_id"].as_str() == Some(*call)
            })
        })
        .count()
}

fn sse(reply: &Reply, role: &str, step: usize, n: usize) -> (String, String) {
    let id = format!("resp_mock_{n}");
    let mut events = vec![json!({"type": "response.created", "response": {"id": id}})];
    let described = match reply {
        Reply::Call {
            name,
            namespace,
            arguments,
        } => {
            let mut item = json!({
                "type": "function_call",
                "call_id": format!("mock_{role}_{step}"),
                "name": name,
                "arguments": arguments.to_string(),
            });
            if let Some(ns) = namespace {
                item["namespace"] = json!(ns);
            }
            events.push(json!({"type": "response.output_item.added", "item": item}));
            events.push(json!({"type": "response.output_item.done", "item": item}));
            format!("{name} {arguments}")
        }
        Reply::Message(text) => {
            let msg_id = format!("msg_mock_{n}");
            let item = json!({"type": "message", "role": "assistant", "id": msg_id,
                "content": [{"type": "output_text", "text": text}]});
            let mut empty = item.clone();
            empty["content"] = json!([]);
            events.push(json!({"type": "response.output_item.added", "item": empty}));
            events.push(json!({"type": "response.output_text.delta", "delta": text,
                "item_id": msg_id, "output_index": 0, "content_index": 0}));
            events.push(json!({"type": "response.output_item.done", "item": item}));
            format!("message {text}")
        }
    };
    events.push(
        json!({"type": "response.completed", "response": {"id": id, "usage": {
        "input_tokens": 1000, "input_tokens_details": {"cached_tokens": 0},
        "output_tokens": 10, "output_tokens_details": {"reasoning_tokens": 0},
        "total_tokens": 1010}}}),
    );
    let text = events
        .iter()
        .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap_or("")))
        .collect();
    (text, described)
}
