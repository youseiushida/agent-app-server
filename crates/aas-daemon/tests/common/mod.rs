//! Helpers of the real-binary tests: temporary config/data folders, the CLI, background
//! daemons and watchdogs that are always cleaned up, a minimal WebSocket client, and the
//! supervisors' PID ledgers.
//!
//! Test helper code starts the binaries with `std::process::Command` (CLAUDE.md allows it for
//! tests); what the binaries start goes through the supervisor.

// Each test crate that includes this module uses a different subset of it, so helpers unused
// by one crate are expected here (not a blanket allow for production code).
#![allow(dead_code)]

use std::io::{BufRead, BufReader};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use aas_daemon::admin_client::{AdminClient, Timeouts};
use aas_daemon::config::Paths;
use aas_supervisor::LedgerEntry;
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

pub const CLI: &str = env!("CARGO_BIN_EXE_agent-app-server");
pub const WATCHDOG: &str = env!("CARGO_BIN_EXE_agent-app-server-daemon");

/// Upper bound for anything a test waits for.
pub const PATIENCE: Duration = Duration::from_secs(60);

/// A free loopback port (bound and released; the tests start right after).
pub fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

pub struct Dirs {
    _tmp: tempfile::TempDir,
    pub config_dir: PathBuf,
    pub data_dir: PathBuf,
    pub root: PathBuf,
    pub listen: SocketAddr,
    pub admin: SocketAddr,
}

impl Dirs {
    pub fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("projects");
        std::fs::create_dir_all(root.join("p")).unwrap();
        let root = dunce::canonicalize(root).unwrap();
        Self {
            config_dir: tmp.path().join("config"),
            data_dir: tmp.path().join("data"),
            root,
            listen: SocketAddr::from(([127, 0, 0, 1], free_port())),
            admin: SocketAddr::from(([127, 0, 0, 1], free_port())),
            _tmp: tmp,
        }
    }

    pub fn paths(&self) -> Paths {
        Paths {
            config_dir: self.config_dir.clone(),
            data_dir: self.data_dir.clone(),
        }
    }

    /// Writes a config with the fake harness running as `agent-app-server fake agent`
    /// processes, short stop graces, and `extra_policy` lines under `[policy]`.
    pub fn write_config(&self, admin_listen: &str, extra_policy: &str) {
        std::fs::create_dir_all(&self.config_dir).unwrap();
        let cli = CLI.replace('\\', "\\\\");
        let root = self.root.display().to_string().replace('\\', "\\\\");
        let text = format!(
            r#"
[server]
listen = "{listen}"
admin_listen = "{admin_listen}"
public_url = "ws://{listen}/v1/ws"
name = "test-pc"

[projects]
roots = ["{root}"]

[policy]
stop_grace = "500ms"
prevent_sleep_while_running = false
{extra_policy}

[logging]
level = "info"

[[harness]]
id = "fake"
kind = "fake"
command = "{cli}"
args = ["fake"]
"#,
            listen = self.listen,
        );
        std::fs::write(self.config_dir.join("config.toml"), text).unwrap();
    }

    pub fn command(&self, program: &str) -> Command {
        let mut c = Command::new(program);
        c.arg("--config-dir")
            .arg(&self.config_dir)
            .arg("--data-dir")
            .arg(&self.data_dir);
        c.env_remove("RUST_LOG");
        c
    }

    /// Runs a CLI subcommand to completion (killed after [`PATIENCE`]).
    pub fn cli(&self, args: &[&str]) -> (i32, String, String) {
        let mut child = self
            .command(CLI)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let status = wait_with_timeout(&mut child, PATIENCE)
            .unwrap_or_else(|| panic!("`agent-app-server {}` did not finish", args.join(" ")));
        let mut out = String::new();
        let mut err = String::new();
        std::io::Read::read_to_string(child.stdout.as_mut().unwrap(), &mut out).unwrap();
        std::io::Read::read_to_string(child.stderr.as_mut().unwrap(), &mut err).unwrap();
        (status.code().unwrap_or(-1), out, err)
    }

    pub fn admin_client(&self) -> AdminClient {
        let token = std::fs::read_to_string(self.config_dir.join("admin-token"))
            .unwrap()
            .trim()
            .to_owned();
        AdminClient::new(
            self.admin,
            Some(token),
            Timeouts {
                connect: Duration::from_secs(5),
                request: Duration::from_secs(30),
            },
        )
    }

    pub fn watchdog_log(&self) -> String {
        std::fs::read_to_string(self.data_dir.join("logs").join("watchdog.log")).unwrap_or_default()
    }

    /// Entries of a supervisor's PID ledger (`supervisor` = the daemon's, `watchdog` = the
    /// watchdog's).
    pub fn ledger(&self, supervisor: &str) -> Vec<LedgerEntry> {
        match std::fs::read(self.data_dir.join(supervisor).join("children.json")) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
            Err(_) => Vec::new(),
        }
    }

    /// Waits until the admin listener answers `status`.
    pub async fn wait_for_daemon(&self) -> aas_protocol::http::AdminStatusResponse {
        let deadline = Instant::now() + PATIENCE;
        loop {
            if self.config_dir.join("admin-token").exists()
                && let Ok(status) = self.admin_client().get("/v1/admin/status").await
            {
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "the daemon did not come up. watchdog.log:\n{}",
                self.watchdog_log()
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Pairs a device through the admin API and `POST /v1/pair`; returns (device id, token).
    pub async fn pair(&self, name: &str) -> (String, String) {
        let code: aas_protocol::http::AdminPairingCodeResponse = self
            .admin_client()
            .post(
                "/v1/admin/pairing-codes",
                &aas_protocol::http::AdminPairingCodeRequest {},
            )
            .await
            .unwrap();
        pair_with_code(self.listen, &code.code, name).await
    }
}

pub async fn pair_with_code(listen: SocketAddr, code: &str, name: &str) -> (String, String) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let body =
        serde_json::to_vec(&json!({"code": code, "deviceName": name, "platform": "test"})).unwrap();
    let mut s = tokio::net::TcpStream::connect(listen).await.unwrap();
    let head = format!(
        "POST /v1/pair HTTP/1.1\r\nHost: x\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
        body.len()
    );
    s.write_all(head.as_bytes()).await.unwrap();
    s.write_all(&body).await.unwrap();
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).await.unwrap();
    let split = buf.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let v: Value = serde_json::from_slice(&buf[split + 4..])
        .unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&buf)));
    (
        v["deviceId"].as_str().unwrap().to_owned(),
        v["token"].as_str().unwrap().to_owned(),
    )
}

pub fn wait_with_timeout(child: &mut Child, timeout: Duration) -> Option<ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return Some(status);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// A background process (daemon or watchdog) that is killed when the test ends, however it
/// ends.
pub struct Background {
    pub child: Child,
    pub name: &'static str,
}

impl Background {
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Waits for the process to exit by itself.
    pub fn wait(&mut self, timeout: Duration) -> ExitStatus {
        wait_with_timeout(&mut self.child, timeout)
            .unwrap_or_else(|| panic!("{} did not exit within {timeout:?}", self.name))
    }
}

impl Drop for Background {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// Starts `agent-app-server run --background` and waits for its ready line.
pub fn start_background_daemon(dirs: &Dirs) -> (Background, Value) {
    let mut child = dirs
        .command(CLI)
        .args(["run", "--background"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut lines = BufReader::new(stdout).lines();
        if let Some(Ok(line)) = lines.next() {
            let _ = tx.send(line);
        }
        // Keep draining so the daemon never blocks on stdout.
        for _ in lines {}
    });
    let background = Background {
        child,
        name: "agent-app-server run --background",
    };
    let line = rx
        .recv_timeout(PATIENCE)
        .expect("the daemon writes its ready line");
    let ready: Value = serde_json::from_str(&line).unwrap();
    (background, ready)
}

/// Starts the watchdog.
pub fn start_watchdog(dirs: &Dirs) -> Background {
    let child = dirs
        .command(WATCHDOG)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    Background {
        child,
        name: "agent-app-server-daemon",
    }
}

pub fn alive(entry: &LedgerEntry) -> bool {
    aas_supervisor::process_is_running(entry.pid, entry.created)
}

/// Waits until every entry's process is gone.
pub fn wait_all_gone(entries: &[LedgerEntry], timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while entries.iter().any(alive) {
        assert!(
            Instant::now() < deadline,
            "still running: {:?}",
            entries.iter().filter(|e| alive(e)).collect::<Vec<_>>()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Waits until `f` returns something.
pub async fn eventually<T>(what: &str, mut f: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + PATIENCE;
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Minimal JSON-RPC client over WebSocket.
pub struct Ws {
    inner: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    next: i64,
}

impl Ws {
    pub async fn connect(listen: SocketAddr, token: &str) -> Self {
        let mut req = format!("ws://{listen}/v1/ws")
            .into_client_request()
            .unwrap();
        req.headers_mut()
            .insert("Authorization", format!("Bearer {token}").parse().unwrap());
        let (inner, _) = tokio_tungstenite::connect_async(req).await.unwrap();
        let mut ws = Self { inner, next: 1 };
        ws.call("initialize", json!({"protocolVersion": 1, "client": {"name": "test", "version": "1", "platform": "test"}})).await;
        ws
    }

    pub async fn call(&mut self, method: &str, params: Value) -> Value {
        let id = self.next;
        self.next += 1;
        let msg = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        self.inner
            .send(Message::text(msg.to_string()))
            .await
            .unwrap();
        loop {
            let frame = tokio::time::timeout(PATIENCE, self.inner.next())
                .await
                .expect("an answer");
            let Some(Ok(Message::Text(t))) = frame else {
                continue;
            };
            let v: Value = serde_json::from_str(&t).unwrap();
            if v["id"] == id {
                assert!(v.get("error").is_none(), "{method}: {v}");
                return v["result"].clone();
            }
        }
    }

    /// Opens the test project and starts a turn that runs far longer than any test.
    pub async fn start_long_turn(&mut self, root: &Path, key: &str) -> String {
        let project = self.call("project/open", json!({"clientRequestId": format!("po-{key}"), "path": root.join("p").display().to_string()})).await;
        let created = self
            .call(
                "thread/create",
                json!({"clientRequestId": format!("tc-{key}"), "projectId": project["project"]["id"], "harnessId": "fake",
                    "input": [{"type": "text", "text": "@sleep 600000"}]}),
            )
            .await;
        created["thread"]["id"].as_str().unwrap().to_owned()
    }
}

/// Waits until a fake agent process of the daemon is on its ledger and alive.
pub async fn agent_process(dirs: &Dirs, exclude: &[u32]) -> LedgerEntry {
    eventually("an agent process", || {
        dirs.ledger("supervisor")
            .into_iter()
            .find(|e| e.label.starts_with("fake[") && !exclude.contains(&e.pid) && alive(e))
    })
    .await
}

/// The error kinds of every turn in the daemon's database (after it stopped).
pub fn turn_error_kinds(dirs: &Dirs) -> Vec<Option<String>> {
    let db = rusqlite::Connection::open_with_flags(
        dirs.data_dir.join("aas.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let mut stmt = db
        .prepare("SELECT error FROM turns ORDER BY started_at")
        .unwrap();
    stmt.query_map([], |r| r.get::<_, Option<String>>(0))
        .unwrap()
        .map(|e| {
            e.unwrap().map(|json| {
                serde_json::from_str::<Value>(&json).unwrap()["kind"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            })
        })
        .collect()
}

/// Top-level windows of `pid` with the end-session window class.
#[cfg(windows)]
pub fn end_session_windows(pid: u32) -> Vec<isize> {
    use windows::Win32::Foundation::{HWND, LPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetClassNameW, GetWindowThreadProcessId,
    };
    use windows::core::BOOL;

    struct Search {
        pid: u32,
        found: Vec<isize>,
    }
    unsafe extern "system" fn visit(hwnd: HWND, lparam: LPARAM) -> BOOL {
        // SAFETY: lparam is the &mut Search passed to EnumWindows below, alive for the call.
        let search = unsafe { &mut *(lparam.0 as *mut Search) };
        let mut pid = 0u32;
        // SAFETY: plain queries on a window handle EnumWindows gave us.
        unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
        if pid == search.pid {
            let mut class = [0u16; 64];
            // SAFETY: as above; the buffer is live.
            let len = unsafe { GetClassNameW(hwnd, &mut class) } as usize;
            if String::from_utf16_lossy(&class[..len]) == "AgentAppServerEndSession" {
                search.found.push(hwnd.0 as isize);
            }
        }
        BOOL(1)
    }
    let mut search = Search {
        pid,
        found: Vec::new(),
    };
    // SAFETY: the callback only runs during this call; `search` outlives it.
    let _ = unsafe { EnumWindows(Some(visit), LPARAM(&mut search as *mut Search as isize)) };
    search.found
}

/// Sends `WM_QUERYENDSESSION` then `WM_ENDSESSION` (logoff) to a window, like Windows does;
/// returns once the window procedure returned.
#[cfg(windows)]
pub fn end_the_session(hwnd: isize) {
    use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{
        ENDSESSION_LOGOFF, SMTO_BLOCK, SendMessageTimeoutW, WM_ENDSESSION, WM_QUERYENDSESSION,
    };
    let hwnd = HWND(hwnd as *mut core::ffi::c_void);
    let mut result = 0usize;
    // SAFETY: SendMessageTimeoutW to a window of another process of this test.
    unsafe {
        SendMessageTimeoutW(
            hwnd,
            WM_QUERYENDSESSION,
            WPARAM(0),
            LPARAM(ENDSESSION_LOGOFF as isize),
            SMTO_BLOCK,
            10_000,
            Some(&mut result),
        );
    }
    assert_eq!(result, 1, "the session may end");
    // SAFETY: as above.
    unsafe {
        SendMessageTimeoutW(
            hwnd,
            WM_ENDSESSION,
            WPARAM(1),
            LPARAM(ENDSESSION_LOGOFF as isize),
            SMTO_BLOCK,
            30_000,
            Some(&mut result),
        );
    }
}
