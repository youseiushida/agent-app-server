//! `aas-test-server` as the Android client's integration tests use it: spawned as a process,
//! driven over stdin, reached through its chaos proxy with the reference client.

mod common;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use aas_adapter_fake::store::SessionStore;
use aas_protocol::events::Event;
use aas_testkit::client::{ClientConfig, ReliableClient};
use aas_testkit::proc::{self, Cleanup, Proc};
use common::{client_events, http_post, mutate};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout};

/// Upper bound for one step (start, restart, a turn); generous because tests run in parallel.
const STEP: Duration = Duration::from_secs(60);

struct TestServerProcess {
    child: Child,
    stdin: ChildStdin,
    stdout: Lines<BufReader<ChildStdout>>,
}

impl TestServerProcess {
    fn spawn(state: &Path) -> Self {
        Self::spawn_with(state, &[])
    }

    /// Spawns with `extra` arguments after the usual ones.
    fn spawn_with(state: &Path, extra: &[&str]) -> Self {
        let mut child = tokio::process::Command::new(proc::bin_path("aas-test-server"))
            .args([
                "--state-dir",
                &state.display().to_string(),
                "--heartbeat-ms",
                "300",
                "--client-timeout-ms",
                "1500",
            ])
            .args(extra)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .expect("spawn aas-test-server");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = BufReader::new(child.stdout.take().expect("stdout")).lines();
        Self {
            child,
            stdin,
            stdout,
        }
    }

    async fn next_event(&mut self) -> Value {
        let line = tokio::time::timeout(STEP, self.stdout.next_line())
            .await
            .expect("aas-test-server printed nothing in time")
            .expect("reading stdout")
            .expect("aas-test-server closed its stdout");
        serde_json::from_str(&line).unwrap_or_else(|e| panic!("not a JSON line: {line:?}: {e}"))
    }

    /// Sends `cmd` and returns everything printed for it; the last event is its `ok`/`error`.
    async fn command(&mut self, cmd: &str) -> Vec<Value> {
        self.stdin
            .write_all(format!("{cmd}\n").as_bytes())
            .await
            .expect("write command");
        self.stdin.flush().await.expect("flush command");
        let mut out = Vec::new();
        loop {
            let event = self.next_event().await;
            let done =
                matches!(event["event"].as_str(), Some("ok" | "error")) && event["cmd"] == cmd;
            out.push(event);
            if done {
                return out;
            }
        }
    }

    async fn ok(&mut self, cmd: &str) -> Vec<Value> {
        let out = self.command(cmd).await;
        assert_eq!(out.last().unwrap()["event"], "ok", "{cmd}: {out:?}");
        out
    }
}

fn ready_of(events: &[Value]) -> Value {
    events
        .iter()
        .find(|e| e["event"] == "ready")
        .cloned()
        .unwrap_or_else(|| panic!("no ready line in {events:?}"))
}

fn client(ready: &Value) -> ReliableClient {
    ReliableClient::start(ClientConfig {
        url: ready["wsUrl"].as_str().unwrap().to_owned(),
        token: ready["token"].as_str().unwrap().to_owned(),
        backoff_max: Duration::from_millis(500),
    })
}

fn proxy_addr(ready: &Value) -> SocketAddr {
    ready["httpUrl"]
        .as_str()
        .unwrap()
        .trim_start_matches("http://")
        .parse()
        .unwrap()
}

async fn wait_connects(client: &ReliableClient, at_least: u32) {
    client.wait_until(STEP, |s| s.connects >= at_least).await;
}

/// Runs `text` as a turn of `thread` and waits until the client saw it complete.
async fn run_turn(client: &ReliableClient, thread: &str, text: &str) {
    let stream = format!("thread:{thread}");
    let r = mutate(
        client,
        "turn/start",
        json!({"threadId": thread, "input": [{"type": "text", "text": text}]}),
        STEP,
    )
    .await;
    let turn = r["turnId"].as_str().expect("turn started").to_owned();
    client
        .wait_until(STEP, |s| {
            client_events(s, &stream).iter().any(
                |e| matches!(&e.event, Event::TurnCompleted { turn: t } if t.id.as_str() == turn),
            )
        })
        .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_test_server_serves_restarts_resets_and_quits_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let agent_pids: PathBuf = state.join("agent-pids");
    let mut cleanup = Cleanup::new();
    cleanup.dir(&agent_pids);
    let mut server = TestServerProcess::spawn(&state);

    let ready = server.next_event().await;
    assert_eq!(ready["event"], "ready", "{ready}");
    for key in [
        "wsUrl",
        "httpUrl",
        "token",
        "deviceId",
        "pairingCode",
        "root",
        "epoch",
    ] {
        assert!(
            ready[key].as_str().is_some_and(|v| !v.is_empty()),
            "ready.{key}: {ready}"
        );
    }
    let root = PathBuf::from(ready["root"].as_str().unwrap());
    assert!(
        root.is_absolute()
            && root.is_dir()
            && root.starts_with(dunce::canonicalize(&state).unwrap()),
        "{root:?}"
    );

    // A turn through the proxy.
    let c = client(&ready);
    wait_connects(&c, 1).await;
    std::fs::create_dir_all(root.join("app")).unwrap();
    let project = mutate(
        &c,
        "project/open",
        json!({"path": root.join("app").display().to_string()}),
        STEP,
    )
    .await;
    let thread = mutate(
        &c,
        "thread/create",
        json!({"projectId": project["project"]["id"], "harnessId": "fake", "input": [{"type": "text", "text": "@text hello from the test server"}]}),
        STEP,
    )
    .await;
    let thread_id = thread["thread"]["id"].as_str().unwrap().to_owned();
    c.subscribe(&format!("thread:{thread_id}"));
    c.wait_until(STEP, |s| {
        client_events(s, &format!("thread:{thread_id}"))
            .iter()
            .any(|e| matches!(e.event, Event::TurnCompleted { .. }))
    })
    .await;
    assert!(
        !proc::recorded(&agent_pids).is_empty(),
        "the agent ran as a recorded process"
    );

    // Chaos: dropped connections come back; the other modes answer.
    let before = c.with_state(|s| s.connects);
    server.ok("chaos drop").await;
    wait_connects(&c, before + 1).await;
    server.ok("chaos delay 20").await;
    server.ok("chaos blackhole").await;
    server.ok("chaos pass").await;
    let unknown = server.command("chaos sideways").await;
    assert_eq!(unknown.last().unwrap()["event"], "error", "{unknown:?}");

    // Restart: same URLs, token and epoch; the client reconnects and the thread goes on.
    let before = c.with_state(|s| s.connects);
    let restarted = ready_of(&server.ok("restart").await);
    for key in ["wsUrl", "httpUrl", "token", "deviceId", "epoch", "root"] {
        assert_eq!(restarted[key], ready[key], "{key} after restart");
    }
    wait_connects(&c, before + 1).await;
    assert_eq!(
        c.with_state(|s| s.epoch.clone()).as_deref(),
        ready["epoch"].as_str()
    );
    run_turn(&c, &thread_id, "after the restart").await;

    // A fresh pairing code pairs a device through the proxy's HTTP endpoint.
    let code = server.ok("pairing-code").await;
    let code = code
        .iter()
        .find(|e| e["event"] == "pairingCode")
        .expect("pairingCode event")["code"]
        .as_str()
        .unwrap()
        .to_owned();
    let body = serde_json::to_vec(
        &json!({"code": code, "deviceName": "second phone", "platform": "test"}),
    )
    .unwrap();
    let (status, resp) = http_post(proxy_addr(&ready), "/v1/pair", &body).await;
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&resp));

    // Reset: a new database (new epoch, new device), same URLs.
    let reset = ready_of(&server.ok("reset").await);
    assert_eq!(reset["wsUrl"], ready["wsUrl"]);
    assert_eq!(reset["httpUrl"], ready["httpUrl"]);
    assert_ne!(reset["epoch"], ready["epoch"]);
    assert_ne!(reset["token"], ready["token"]);
    drop(c);
    let fresh = client(&reset);
    wait_connects(&fresh, 1).await;
    assert_eq!(
        fresh.with_state(|s| s.epoch.clone()).as_deref(),
        reset["epoch"].as_str()
    );
    let projects = mutate(
        &fresh,
        "project/open",
        json!({"path": root.join("app").display().to_string()}),
        STEP,
    )
    .await;
    assert_ne!(
        projects["project"]["id"], project["project"]["id"],
        "the reset forgot the old project"
    );
    drop(fresh);

    // Quit: exit code 0, and no agent process survives.
    let agents: Vec<Proc> = proc::recorded(&agent_pids);
    assert!(
        agents.len() >= 2,
        "one agent per daemon generation that ran a turn: {agents:?}"
    );
    server.ok("quit").await;
    let status = tokio::time::timeout(STEP, server.child.wait())
        .await
        .expect("exits after quit")
        .expect("exit status");
    assert_eq!(status.code(), Some(0));
    let survivors =
        tokio::task::spawn_blocking(move || proc::wait_all_dead(&agents, Duration::from_secs(10)))
            .await
            .unwrap();
    assert!(
        survivors.is_empty(),
        "agent processes outlived the test server: {survivors:?}"
    );
}

/// Background work through the test server (real agent processes): a task that runs until it is
/// stopped keeps the agent's process however long the thread is idle, is stopped through
/// `backgroundTask/stop`, and the idle stop follows; nothing outlives the server.
#[tokio::test(flavor = "multi_thread")]
async fn background_work_keeps_the_agent_until_it_is_stopped() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let agent_pids: PathBuf = state.join("agent-pids");
    let mut cleanup = Cleanup::new();
    cleanup.dir(&agent_pids);
    let mut server = TestServerProcess::spawn_with(
        &state,
        &[
            "--idle-process-ttl-ms",
            "400",
            "--background-stop-confirm-ms",
            "5000",
        ],
    );
    let ready = server.next_event().await;
    let root = PathBuf::from(ready["root"].as_str().unwrap());
    let c = client(&ready);
    wait_connects(&c, 1).await;
    std::fs::create_dir_all(root.join("bg")).unwrap();
    let project = mutate(
        &c,
        "project/open",
        json!({"path": root.join("bg").display().to_string()}),
        STEP,
    )
    .await;
    let thread = mutate(
        &c,
        "thread/create",
        json!({"projectId": project["project"]["id"], "harnessId": "fake",
            "input": [{"type": "text", "text": "@bg dev kind=shell ms=0 npm run dev\n@text the server runs"}]}),
        STEP,
    )
    .await;
    let thread_id = thread["thread"]["id"].as_str().unwrap().to_owned();
    let stream = format!("thread:{thread_id}");
    c.subscribe(&stream);
    c.wait_until(STEP, |s| {
        client_events(s, &stream)
            .iter()
            .any(|e| matches!(e.event, Event::TurnCompleted { .. }))
    })
    .await;
    // Far longer than the idle time: the agent keeps running.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let read = mutate(&c, "thread/read", json!({"threadId": thread_id}), STEP).await;
    assert_eq!(read["thread"]["status"], "ready", "{read}");
    assert_eq!(read["thread"]["background"]["running"], 1, "{read}");
    let task = &read["backgroundTasks"][0];
    assert_eq!(task["status"], "running");
    assert_eq!(task["kind"], "shell");
    let agents: Vec<Proc> = proc::recorded(&agent_pids);
    assert_eq!(agents.len(), 1, "{agents:?}");

    let stop = mutate(
        &c,
        "backgroundTask/stop",
        json!({"threadId": thread_id, "taskId": task["id"]}),
        STEP,
    )
    .await;
    assert!(stop["task"]["stopRequestedAt"].is_number(), "{stop}");
    c.wait_until(STEP, |s| {
        client_events(s, &stream).iter().any(|e| {
            matches!(&e.event, Event::BackgroundTaskUpdated { task }
                if task.status == aas_protocol::BackgroundTaskStatus::Stopped)
        })
    })
    .await;
    // Nothing keeps it busy any more: the idle stop follows.
    c.wait_until(STEP, |s| {
        client_events(s, &stream).iter().any(|e| {
            matches!(&e.event, Event::ThreadUpdated { thread }
                if thread.status == aas_protocol::ThreadStatus::Idle)
        })
    })
    .await;
    drop(c);
    server.ok("quit").await;
    let status = tokio::time::timeout(STEP, server.child.wait())
        .await
        .expect("exits after quit")
        .expect("exit status");
    assert_eq!(status.code(), Some(0));
    let survivors =
        tokio::task::spawn_blocking(move || proc::wait_all_dead(&agents, Duration::from_secs(10)))
            .await
            .unwrap();
    assert!(survivors.is_empty(), "{survivors:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn eof_on_stdin_quits_and_a_later_process_reuses_the_device() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let mut first = TestServerProcess::spawn(&state);
    let ready = first.next_event().await;
    drop(first.stdin);
    let status = tokio::time::timeout(STEP, first.child.wait())
        .await
        .expect("exits on EOF")
        .expect("exit status");
    assert_eq!(status.code(), Some(0));

    let mut second = TestServerProcess::spawn(&state);
    let again = second.next_event().await;
    assert_eq!(
        again["token"], ready["token"],
        "the paired device is kept with the state dir"
    );
    assert_eq!(again["epoch"], ready["epoch"]);
    second.ok("quit").await;
    let status = tokio::time::timeout(STEP, second.child.wait())
        .await
        .expect("exits after quit")
        .expect("exit status");
    assert_eq!(status.code(), Some(0));

    let bad = tokio::process::Command::new(proc::bin_path("aas-test-server"))
        .stdin(Stdio::null())
        .output()
        .await
        .unwrap();
    assert_ne!(bad.status.code(), Some(0), "--state-dir is required");
}

/// The session store of the fake agent, as the test server configures it.
fn native_store(ready: &Value) -> SessionStore {
    SessionStore::new(
        ready["nativeSessionsDir"]
            .as_str()
            .expect("nativeSessionsDir"),
    )
}

/// `native/list` of the fake harness in a project.
async fn native_list(client: &ReliableClient, project_id: &Value) -> Value {
    let params = json!({"projectId": project_id, "harnessId": "fake"});
    mutate(client, "native/list", params, STEP).await["sessions"].clone()
}

fn session_titles(sessions: &Value) -> Vec<String> {
    sessions
        .as_array()
        .expect("a session list")
        .iter()
        .map(|s| s["title"].as_str().unwrap_or_default().to_owned())
        .collect()
}

/// "Import PC sessions" and fork through the real server: the seeded native sessions are
/// listed and imported, the imported thread resumes its native session, a fork branches a new
/// native session off it, and `native-session` records more sessions on demand.
#[tokio::test(flavor = "multi_thread")]
async fn native_sessions_are_listed_imported_resumed_and_forked() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let agent_pids: PathBuf = state.join("agent-pids");
    let mut cleanup = Cleanup::new();
    cleanup.dir(&agent_pids);
    let mut server = TestServerProcess::spawn(&state);
    let ready = server.next_event().await;
    assert_eq!(ready["event"], "ready", "{ready}");

    let root = PathBuf::from(ready["root"].as_str().unwrap());
    let native_project = PathBuf::from(ready["nativeProject"].as_str().expect("nativeProject"));
    assert!(
        native_project.is_dir() && native_project.starts_with(&root),
        "{native_project:?}"
    );
    let store = native_store(&ready);
    assert!(store.dir().is_dir(), "{:?}", store.dir());
    let mut seeded = session_titles(&ready["nativeSessions"]);
    seeded.sort();
    assert_eq!(seeded, vec!["Check the tests", "Explain the build"]);

    let c = client(&ready);
    wait_connects(&c, 1).await;
    let project = mutate(
        &c,
        "project/open",
        json!({"path": native_project.display().to_string()}),
        STEP,
    )
    .await;
    let project_id = project["project"]["id"].clone();
    let sessions = native_list(&c, &project_id).await;
    assert_eq!(sessions.as_array().unwrap().len(), 2, "{sessions}");
    assert!(
        sessions
            .as_array()
            .unwrap()
            .iter()
            .all(|s| s.get("importedThreadId").is_none()),
        "{sessions}"
    );
    let explain = sessions
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["title"] == "Explain the build")
        .expect("the seeded session")["nativeSessionId"]
        .as_str()
        .unwrap()
        .to_owned();

    // Import: the history arrives as completed turns.
    let imported = mutate(
        &c,
        "native/import",
        json!({"projectId": project_id, "harnessId": "fake", "nativeSessionId": explain}),
        STEP,
    )
    .await;
    let thread_id = imported["thread"]["id"].as_str().unwrap().to_owned();
    let read = mutate(&c, "thread/read", json!({"threadId": thread_id}), STEP).await;
    let turns = read["turns"].as_array().unwrap();
    assert_eq!(turns.len(), 2, "{read}");
    assert!(turns.iter().all(|t| t["status"] == "completed"), "{read}");
    let kinds: Vec<&str> = read["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|i| i["kind"].as_str())
        .collect();
    for kind in [
        "userMessage",
        "reasoning",
        "commandExecution",
        "agentMessage",
        "plan",
    ] {
        assert!(kinds.contains(&kind), "{kind} in {kinds:?}");
    }
    let sessions = native_list(&c, &project_id).await;
    let entry = sessions
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["nativeSessionId"] == explain.as_str())
        .unwrap();
    assert_eq!(entry["importedThreadId"], thread_id.as_str(), "{sessions}");

    // The imported thread goes on in its native session.
    c.subscribe(&format!("thread:{thread_id}"));
    run_turn(&c, &thread_id, "@text continued on the phone").await;
    let transcript = store.read(&explain).expect("the imported session");
    assert_eq!(transcript.turns.len(), 3, "resumed, not replaced");

    // A fork branches a new native session off it on its first turn.
    let fork = mutate(&c, "thread/fork", json!({"threadId": thread_id}), STEP).await;
    let fork_id = fork["thread"]["id"].as_str().unwrap().to_owned();
    c.subscribe(&format!("thread:{fork_id}"));
    run_turn(&c, &fork_id, "@text only in the fork").await;
    let sessions = native_list(&c, &project_id).await;
    assert_eq!(sessions.as_array().unwrap().len(), 3, "{sessions}");
    let branch = sessions
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["importedThreadId"] == fork_id.as_str())
        .unwrap_or_else(|| panic!("the fork's native session: {sessions}"))["nativeSessionId"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_ne!(branch, explain);
    let branched = store.read(&branch).expect("the fork's session");
    assert_eq!(branched.forked_from.as_deref(), Some(explain.as_str()));
    assert_eq!(
        branched.turns.len(),
        4,
        "the source's three turns and its own"
    );
    assert_eq!(
        store.read(&explain).unwrap().turns.len(),
        3,
        "the source is unchanged"
    );

    // More sessions on demand, in any folder under the root.
    let out = server
        .ok("native-session later/app Review the diff\\n@exec git diff\\n@text Looks fine.")
        .await;
    let recorded = out
        .iter()
        .find(|e| e["event"] == "nativeSession")
        .unwrap_or_else(|| panic!("no nativeSession line in {out:?}"));
    let cwd = PathBuf::from(recorded["cwd"].as_str().unwrap());
    assert_eq!(cwd, root.join("later").join("app"));
    assert_eq!(recorded["title"], "Review the diff");
    let other = mutate(
        &c,
        "project/open",
        json!({"path": cwd.display().to_string()}),
        STEP,
    )
    .await;
    let other_sessions = native_list(&c, &other["project"]["id"]).await;
    assert_eq!(
        other_sessions[0]["nativeSessionId"], recorded["nativeSessionId"],
        "{other_sessions}"
    );
    for bad in [
        "native-session",
        "native-session app",
        "native-session ../outside hi",
    ] {
        let out = server.command(bad).await;
        assert_eq!(out.last().unwrap()["event"], "error", "{bad}: {out:?}");
    }

    // Native sessions are the CLI's data: a reset keeps them (and nothing is imported yet).
    let reset = ready_of(&server.ok("reset").await);
    assert_eq!(reset["nativeSessions"].as_array().unwrap().len(), 3);
    drop(c);

    let agents: Vec<Proc> = proc::recorded(&agent_pids);
    server.ok("quit").await;
    let status = tokio::time::timeout(STEP, server.child.wait())
        .await
        .expect("exits after quit")
        .expect("exit status");
    assert_eq!(status.code(), Some(0));
    let survivors =
        tokio::task::spawn_blocking(move || proc::wait_all_dead(&agents, Duration::from_secs(10)))
            .await
            .unwrap();
    assert!(
        survivors.is_empty(),
        "agent processes outlived the test server: {survivors:?}"
    );
}

/// The completed turn `turn` of `thread` in what the client applied.
fn completed_turn(
    state: &aas_testkit::client::ClientState,
    thread: &str,
    turn: &str,
) -> Option<aas_protocol::Turn> {
    let stream = format!("thread:{thread}");
    client_events(state, &stream)
        .iter()
        .find_map(|e| match &e.event {
            Event::TurnCompleted { turn: t } if t.id.as_str() == turn => Some(t.clone()),
            _ => None,
        })
}

/// Starts `text` in `thread` and waits for its completion (whatever its status).
async fn finished_turn(c: &ReliableClient, thread: &str, text: &str) -> aas_protocol::Turn {
    let r = mutate(
        c,
        "turn/start",
        json!({"threadId": thread, "input": [{"type": "text", "text": text}]}),
        STEP,
    )
    .await;
    let turn = r["turnId"].as_str().expect("turn started").to_owned();
    c.wait_until(STEP, |s| completed_turn(s, thread, &turn).is_some())
        .await;
    c.with_state(|s| completed_turn(s, thread, &turn))
        .expect("the turn completed")
}

/// The extended features through the real server with a real agent process: a session held by
/// another process makes the next turn fail as `resumeFailed` with the agent's own words (its
/// coloured stderr without escapes), a fork at a turn still runs, a typed session switch is
/// refused, and the harness status and side questions answer.
#[tokio::test(flavor = "multi_thread")]
async fn held_sessions_forks_at_turns_and_side_questions_through_the_real_server() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let agent_pids: PathBuf = state.join("agent-pids");
    let mut cleanup = Cleanup::new();
    cleanup.dir(&agent_pids);
    let mut server = TestServerProcess::spawn(&state);
    let ready = server.next_event().await;
    assert_eq!(ready["event"], "ready", "{ready}");
    let root = PathBuf::from(ready["root"].as_str().unwrap());
    let folder = root.join("extended");
    std::fs::create_dir_all(&folder).unwrap();
    let c = client(&ready);
    wait_connects(&c, 1).await;
    let features =
        mutate(&c, "harness/list", json!({}), STEP).await["harnesses"][0]["features"].clone();
    assert_eq!(features["forkAtTurn"], true, "{features}");
    assert_eq!(features["forkWhileHeld"], true, "{features}");
    let project = mutate(
        &c,
        "project/open",
        json!({"path": folder.display().to_string()}),
        STEP,
    )
    .await;
    let thread = mutate(
        &c,
        "thread/create",
        json!({"projectId": project["project"]["id"], "harnessId": "fake"}),
        STEP,
    )
    .await["thread"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    c.subscribe(&format!("thread:{thread}"));
    let first = finished_turn(&c, &thread, "first").await;
    assert!(first.forkable, "{first:?}");
    finished_turn(&c, &thread, "second").await;

    // A typed session switch never reaches the agent.
    let crid = c.mutate(
        "turn/start",
        json!({"threadId": thread, "input": [{"type": "text", "text": "/fake-clear"}]}),
    );
    let refused = c.result(&crid, STEP).await.unwrap_err();
    assert_eq!(
        refused.kind(),
        Some(aas_protocol::ErrorKind::SessionSwitchingCommand)
    );

    // Held by another process: the resume fails with the agent's own words.
    let native = mutate(&c, "thread/get", json!({"threadId": thread}), STEP).await["thread"]
        ["nativeSessionId"]
        .as_str()
        .unwrap()
        .to_owned();
    mutate(&c, "thread/stop", json!({"threadId": thread}), STEP).await;
    server.ok(&format!("hold-session {native}")).await;
    let failed = finished_turn(&c, &thread, "third").await;
    let error = failed.error.expect("the turn failed");
    assert_eq!(error.kind, "resumeFailed", "{error:?}");
    assert!(
        error.message.contains("is held by another process"),
        "{error:?}"
    );
    assert!(!error.message.contains('\u{1b}'), "{error:?}");

    // A fork at the first turn still runs, beside the held session.
    let fork = mutate(
        &c,
        "thread/fork",
        json!({"threadId": thread, "atTurnId": first.id}),
        STEP,
    )
    .await["thread"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    c.subscribe(&format!("thread:{fork}"));
    let forked = finished_turn(&c, &fork, "@text in the fork\n@sleep 200").await;
    assert_eq!(
        forked.status,
        aas_protocol::TurnStatus::Completed,
        "{forked:?}"
    );
    let status = mutate(&c, "thread/harnessStatus", json!({"threadId": fork}), STEP).await;
    assert_eq!(status["live"], true, "{status}");
    let answer = mutate(
        &c,
        "thread/sideQuestion",
        json!({"threadId": fork, "question": "still there?"}),
        STEP,
    )
    .await;
    assert_eq!(answer["answer"], "side answer: still there?");

    // Released, the thread resumes its session again.
    server.ok(&format!("release-session {native}")).await;
    let resumed = finished_turn(&c, &thread, "fourth").await;
    assert_eq!(resumed.status, aas_protocol::TurnStatus::Completed);
    for bad in ["hold-session", "hold-session no-such-session"] {
        let out = server.command(bad).await;
        assert_eq!(out.last().unwrap()["event"], "error", "{bad}: {out:?}");
    }
    drop(c);

    let agents: Vec<Proc> = proc::recorded(&agent_pids);
    server.ok("quit").await;
    let status = tokio::time::timeout(STEP, server.child.wait())
        .await
        .expect("exits after quit")
        .expect("exit status");
    assert_eq!(status.code(), Some(0));
    let survivors =
        tokio::task::spawn_blocking(move || proc::wait_all_dead(&agents, Duration::from_secs(10)))
            .await
            .unwrap();
    assert!(
        survivors.is_empty(),
        "agent processes outlived the test server: {survivors:?}"
    );
}
