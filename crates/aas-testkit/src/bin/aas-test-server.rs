//! `aas-test-server`: the real daemon (engine + transport, with the fake harness running
//! `aas-dummy-agent agent` as supervised processes) behind a [`ChaosProxy`], driven over
//! stdin/stdout. It exists for the Android client's integration tests (see
//! `docs/design.md` §16).
//!
//! ```text
//! aas-test-server --state-dir <dir> [--heartbeat-ms 300] [--client-timeout-ms 1500]
//!                 [--idle-process-ttl-ms <ms>] [--background-progress-ms <ms>]
//!                 [--background-stop-confirm-ms <ms>] [--max-inline-output-bytes <n>]
//! ```
//!
//! The last four set the daemon's `policy.idle_process_ttl`, `background_progress_interval`,
//! `background_stop_confirm_timeout` and `max_inline_output_bytes` (defaults: the daemon's), so
//! that a client test can see an idle agent stopped, coalesced progress, an unconfirmed stop,
//! and output cut at the inline limit (its whole in a blob) within its patience. The fake
//! agent's background work (`@bg`, with streamed output `output=`; wakeups `@wakeup`; questions
//! outside turns `@dialog`, see `aas_adapter_fake::agent`) runs in the prompts like every other
//! scenario.
//!
//! Output (stdout, one JSON object per line, flushed):
//! * `{"event":"ready","wsUrl":…,"httpUrl":…,"token":…,"deviceId":…,"pairingCode":…,"root":…,"epoch":…,
//!   "nativeSessionsDir":…,"nativeProject":…,"nativeSessions":[{"nativeSessionId":…,"title":…},…]}`
//!   once the server accepts connections (again after `restart` and `reset`);
//! * `{"event":"pairingCode","code":…}` for `pairing-code`;
//! * `{"event":"nativeSession","nativeSessionId":…,"cwd":…,"title":…}` for `native-session`;
//! * `{"event":"ok","cmd":<command line>}` or `{"event":"error","cmd":<command line>,"message":…}`
//!   after every command, as its last line.
//!
//! Commands (stdin, one per line): `chaos pass`, `chaos drop`, `chaos blackhole`,
//! `chaos delay <ms>`, `restart`, `reset`, `pairing-code`,
//! `native-session <folder> <prompt…>`, `hold-session <nativeSessionId>`,
//! `release-session <nativeSessionId>`, `quit` (EOF on stdin is `quit`).
//!
//! The proxy keeps its port for the life of the process; after a restart it forwards to the
//! new server. Every agent process records itself in `<state-dir>/agent-pids` (see
//! `aas-dummy-agent`), so a test can check that none outlives the server.
//!
//! # Native sessions ("PC sessions" to import, and forks)
//!
//! The fake harness keeps its sessions in `<state-dir>/fake-sessions` (`nativeSessionsDir`),
//! so it offers `fork` and `nativeSessions` like the real CLIs. The first start on a state dir
//! records [`SEEDED_SESSIONS`] there, as if someone had used the CLI on the PC in the project
//! folder `<root>/pc-sessions` (`nativeProject`): open that folder as a project, and
//! `native/list` / `native/import` find them. `nativeSessions` lists every session recorded
//! for `nativeProject` (newest first) at the time of the ready line. Sessions are the CLI's
//! data, not the daemon's: `restart` and `reset` keep them.
//!
//! `native-session <folder> <prompt…>` records one more session: `<folder>` is relative to
//! `root` (created when missing), and the prompt is the rest of the line, where `\n` stands
//! for a line break (so a prompt can hold several scenario directives, see
//! `aas_adapter_fake::agent`). Requests are answered by a scripted user (approvals allowed,
//! first choices picked).
//!
//! `hold-session <nativeSessionId>` marks a native session as held by another process (like a
//! Codex thread open in Codex desktop): resuming it fails (the thread's next turn ends as
//! `resumeFailed`, with the agent's coloured error on its stderr), forking it works.
//! `release-session <nativeSessionId>` ends the hold.
//!
//! # Extended features
//!
//! The fake harness offers every feature of `Harness.features` (forks at a turn, forks of held
//! sessions, renames, the harness status, side questions, moving running work to the
//! background, plan mode with proposed plans, fast mode with `fake-fast`, the project trust
//! decision) and a session-switching command with an alias (`/fake-clear`, `/fake-reset`). The
//! scenarios that exercise them (`@switch-session`, `@permission`, `@effort`, `@plan-mode`,
//! `@proposed-plan`, `@fast-state`, `@rename`, `@editor`, `@tool`, `@refuse-steers`,
//! `@await-steer`, `@trust`, `@stderr`) are in the directive table of `aas_adapter_fake::agent`.
//! The model `fake-lite` runs in the permission mode `ask` only (`Model.permissionModes`).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use aas_adapter_fake::FakeAdapter;
use aas_adapter_fake::agent::record_session;
use aas_adapter_fake::store::SessionStore;
use aas_core::{Engine, EngineConfig, HarnessRegistry, Policy};
use aas_harness::{AdapterContext, HarnessConfig, HarnessKind};
use aas_server::{Server, ServerOptions};
use aas_supervisor::{Supervisor, resolve_program};
use aas_testkit::chaos::{ChaosProxy, Mode};
use anyhow::{Context, bail};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

/// Id of the fake harness the server offers.
const HARNESS_ID: &str = "fake";
/// Name of the device the server pairs for the tests.
const DEVICE_NAME: &str = "aas-test-client";
/// Environment variable the dummy agent records itself under (see `aas-dummy-agent`).
const AGENT_PID_DIR_ENV: &str = "AAS_DUMMY_AGENT_PID_DIR";
/// Default `--heartbeat-ms`: short, so client watchdogs react within a test's patience.
const DEFAULT_HEARTBEAT_MS: u64 = 300;
/// Default `--client-timeout-ms`: five heartbeats.
const DEFAULT_CLIENT_TIMEOUT_MS: u64 = 1500;
/// Staged-stop grace of the agents: the fake agent exits at once on EOF, so a short grace
/// keeps restarts fast.
const STOP_GRACE: Duration = Duration::from_millis(500);
/// Interrupt grace of the agents (the fake agent honours interrupts immediately).
const INTERRUPT_GRACE: Duration = Duration::from_millis(1500);
/// Upper bound for recording one native session (`native-session`, the seeds). Recording
/// runs the fake agent in memory and takes milliseconds; only a prompt that waits (`@sleep`,
/// `@hang`) can reach it.
const RECORD_TIMEOUT: Duration = Duration::from_secs(60);
/// Folder of the seeded native sessions, under the project root.
const NATIVE_PROJECT: &str = "pc-sessions";
/// The native sessions recorded on the first start of a state dir: the turns (prompts in the
/// fake agent's scenario language) of each session. Their titles are the first lines.
const SEEDED_SESSIONS: &[&[&str]] = &[
    &[
        "Explain the build\n@reason The workspace is built with cargo.\n@exec cargo build\n@text It is a cargo workspace.",
        "Plan the release\n@plan\n@text The plan is ready.",
    ],
    &["Check the tests\n@approve cargo test\n@text All tests pass."],
];

struct Args {
    state_dir: PathBuf,
    heartbeat: Duration,
    client_timeout: Duration,
    idle_process_ttl: Option<Duration>,
    background_progress_interval: Option<Duration>,
    background_stop_confirm_timeout: Option<Duration>,
    max_inline_output_bytes: Option<usize>,
}

fn parse_args() -> anyhow::Result<Args> {
    let mut state_dir = None;
    let mut heartbeat_ms = DEFAULT_HEARTBEAT_MS;
    let mut client_timeout_ms = DEFAULT_CLIENT_TIMEOUT_MS;
    let mut idle_process_ttl = None;
    let mut background_progress_interval = None;
    let mut background_stop_confirm_timeout = None;
    let mut max_inline_output_bytes = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = |name: &str| args.next().with_context(|| format!("{name} needs a value"));
        let mut millis = |name: &str| -> anyhow::Result<Option<Duration>> {
            let v: u64 = value(name)?
                .parse()
                .with_context(|| format!("{name} must be a number"))?;
            Ok(Some(Duration::from_millis(v)))
        };
        match arg.as_str() {
            "--idle-process-ttl-ms" => idle_process_ttl = millis("--idle-process-ttl-ms")?,
            "--background-progress-ms" => {
                background_progress_interval = millis("--background-progress-ms")?
            }
            "--background-stop-confirm-ms" => {
                background_stop_confirm_timeout = millis("--background-stop-confirm-ms")?
            }
            "--max-inline-output-bytes" => {
                max_inline_output_bytes = Some(
                    value("--max-inline-output-bytes")?
                        .parse()
                        .context("--max-inline-output-bytes must be a number")?,
                )
            }
            "--state-dir" => state_dir = Some(PathBuf::from(value("--state-dir")?)),
            "--heartbeat-ms" => {
                heartbeat_ms = value("--heartbeat-ms")?
                    .parse()
                    .context("--heartbeat-ms must be a number")?
            }
            "--client-timeout-ms" => {
                client_timeout_ms = value("--client-timeout-ms")?
                    .parse()
                    .context("--client-timeout-ms must be a number")?
            }
            other => bail!("unknown argument {other}"),
        }
    }
    let state_dir = state_dir.context("--state-dir <dir> is required")?;
    std::fs::create_dir_all(&state_dir)
        .with_context(|| format!("creating {}", state_dir.display()))?;
    let state_dir = dunce::canonicalize(&state_dir)
        .with_context(|| format!("resolving {}", state_dir.display()))?;
    Ok(Args {
        state_dir,
        heartbeat: Duration::from_millis(heartbeat_ms),
        client_timeout: Duration::from_millis(client_timeout_ms),
        idle_process_ttl,
        background_progress_interval,
        background_stop_confirm_timeout,
        max_inline_output_bytes,
    })
}

/// Folders inside the state dir.
struct Paths {
    state: PathBuf,
}

impl Paths {
    fn data(&self) -> PathBuf {
        self.state.join("data")
    }
    fn projects(&self) -> PathBuf {
        self.state.join("projects")
    }
    fn agent_pids(&self) -> PathBuf {
        self.state.join("agent-pids")
    }
    /// The fake agent's session store (its native sessions).
    fn native_sessions(&self) -> PathBuf {
        self.state.join("fake-sessions")
    }
    /// The paired device's id and token (kept so that a later process on the same state dir
    /// reuses the device).
    fn device_file(&self) -> PathBuf {
        self.state.join("test-device.json")
    }
    fn database_files(&self) -> Vec<PathBuf> {
        ["aas.db", "aas.db-wal", "aas.db-shm"]
            .iter()
            .map(|f| self.data().join(f))
            .collect()
    }
}

/// One running daemon (engine and transport).
struct Daemon {
    engine: Arc<Engine>,
    stop: oneshot::Sender<()>,
    serve: JoinHandle<std::io::Result<()>>,
}

#[derive(Clone)]
struct Device {
    id: String,
    token: String,
}

struct TestServer {
    paths: Paths,
    policy: Policy,
    /// Started with the first daemon; keeps its port for the life of the process.
    proxy: Option<ChaosProxy>,
    root: PathBuf,
    daemon: Option<Daemon>,
    device: Option<Device>,
}

fn emit(value: Value) -> anyhow::Result<()> {
    let mut out = std::io::stdout().lock();
    serde_json::to_writer(&mut out, &value)?;
    out.write_all(b"\n")?;
    out.flush()?;
    Ok(())
}

fn exe_dir() -> anyhow::Result<PathBuf> {
    let exe = std::env::current_exe().context("locating the executable")?;
    Ok(exe
        .parent()
        .context("the executable has no folder")?
        .to_path_buf())
}

impl TestServer {
    async fn new(args: Args) -> anyhow::Result<Self> {
        let paths = Paths {
            state: args.state_dir,
        };
        std::fs::create_dir_all(paths.projects())?;
        let root = dunce::canonicalize(paths.projects())?;
        seed_native_sessions(&paths.native_sessions(), &root.join(NATIVE_PROJECT)).await?;
        let defaults = Policy::default();
        let policy = Policy {
            heartbeat_interval: args.heartbeat,
            client_timeout: args.client_timeout,
            stop_grace: STOP_GRACE,
            interrupt_grace: INTERRUPT_GRACE,
            prevent_sleep_while_running: false,
            idle_process_ttl: args.idle_process_ttl.unwrap_or(defaults.idle_process_ttl),
            background_progress_interval: args
                .background_progress_interval
                .unwrap_or(defaults.background_progress_interval),
            background_stop_confirm_timeout: args
                .background_stop_confirm_timeout
                .unwrap_or(defaults.background_stop_confirm_timeout),
            max_inline_output_bytes: args
                .max_inline_output_bytes
                .unwrap_or(defaults.max_inline_output_bytes),
            ..defaults.clone()
        };
        policy.validate().map_err(anyhow::Error::msg)?;
        Ok(Self {
            paths,
            policy,
            proxy: None,
            root,
            daemon: None,
            device: None,
        })
    }

    fn proxy(&self) -> anyhow::Result<&ChaosProxy> {
        self.proxy.as_ref().context("the proxy is not running")
    }

    fn ws_url(&self) -> anyhow::Result<String> {
        Ok(format!("ws://{}/v1/ws", self.proxy()?.addr()))
    }

    fn http_url(&self) -> anyhow::Result<String> {
        Ok(format!("http://{}", self.proxy()?.addr()))
    }

    async fn start(&mut self) -> anyhow::Result<()> {
        let data = self.paths.data();
        let supervisor = Supervisor::new(&data.join("supervisor"), self.policy.supervisor_policy())
            .context("starting the supervisor")?;
        let sweep = supervisor.sweep_orphans();
        if !sweep.terminated.is_empty() || !sweep.failed.is_empty() {
            tracing::warn!(
                terminated = sweep.terminated.len(),
                failed = sweep.failed.len(),
                "left-over agent processes were cleaned up"
            );
        }
        let agent = exe_dir()?.join(if cfg!(windows) {
            "aas-dummy-agent.exe"
        } else {
            "aas-dummy-agent"
        });
        if !agent.exists() {
            bail!(
                "{} is missing: aas-dummy-agent must sit next to aas-test-server",
                agent.display()
            );
        }
        let harness = HarnessConfig {
            id: HARNESS_ID.to_owned(),
            kind: HarnessKind::Fake,
            display_name: Some("Fake agent".to_owned()),
            command: agent.display().to_string(),
            args: Vec::new(),
            env: [(
                AGENT_PID_DIR_ENV.to_owned(),
                self.paths.agent_pids().display().to_string(),
            )]
            .into_iter()
            .collect(),
            options: json!({
                "mode": "process",
                "sessionsDir": self.paths.native_sessions().display().to_string(),
            }),
        };
        let ctx = AdapterContext {
            supervisor: supervisor.clone(),
            state_dir: data.join("adapters").join(HARNESS_ID),
            policy: self.policy.adapter_policy(),
        };
        let registry = HarnessRegistry::new(vec![Arc::new(FakeAdapter::new(harness, ctx))]);
        let config = EngineConfig {
            data_dir: data,
            server_name: "aas-test-server".to_owned(),
            hostname: "aas-test-server".to_owned(),
            project_roots: vec![self.root.clone()],
            policy: self.policy.clone(),
            heuristics: Default::default(),
            git: resolve_program("git").ok(),
        };
        let engine = Engine::start(config, registry, supervisor)
            .await
            .context("starting the engine")?;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .context("listening")?;
        let listen = listener.local_addr()?;
        match &self.proxy {
            Some(proxy) => proxy.set_upstream(listen),
            None => {
                self.proxy = Some(
                    ChaosProxy::start(listen)
                        .await
                        .context("starting the chaos proxy")?,
                )
            }
        }
        let server = Server::new(
            engine.clone(),
            ServerOptions {
                listen,
                public_url: Some(self.ws_url()?),
                admin_token: aas_core::auth::new_token(),
            },
        );
        let (stop, stopped) = oneshot::channel::<()>();
        let serve = tokio::spawn(server.run(listener, async move {
            // A dropped sender (the test server itself going away) also stops the transport.
            let _ = stopped.await;
        }));
        self.daemon = Some(Daemon {
            engine,
            stop,
            serve,
        });
        self.ensure_device().await
    }

    /// Stops the daemon like `agent-app-server stop`: clients get `server/shuttingDown`, then
    /// every agent process is stopped.
    async fn stop(&mut self) -> anyhow::Result<()> {
        let Some(daemon) = self.daemon.take() else {
            return Ok(());
        };
        let _ = daemon.stop.send(());
        daemon
            .serve
            .await
            .context("the transport task failed")?
            .context("serving")?;
        daemon.engine.shutdown(false).await;
        // Frees the database files: the next start (or `reset`) opens them afresh even while a
        // finished connection task still holds the old engine.
        daemon
            .engine
            .close()
            .await
            .context("closing the database")?;
        Ok(())
    }

    fn engine(&self) -> anyhow::Result<&Arc<Engine>> {
        Ok(&self
            .daemon
            .as_ref()
            .context("the daemon is not running")?
            .engine)
    }

    /// Reuses the recorded device while its token is valid, otherwise pairs a new one.
    async fn ensure_device(&mut self) -> anyhow::Result<()> {
        let recorded: Option<Device> = match std::fs::read(self.paths.device_file()) {
            Ok(bytes) => {
                let v: Value =
                    serde_json::from_slice(&bytes).context("reading the recorded test device")?;
                match (v["deviceId"].as_str(), v["token"].as_str()) {
                    (Some(id), Some(token)) => Some(Device {
                        id: id.to_owned(),
                        token: token.to_owned(),
                    }),
                    _ => bail!("{} is malformed", self.paths.device_file().display()),
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e).context("reading the recorded test device"),
        };
        if let Some(device) = recorded
            && self.engine()?.authenticate(&device.token).await?.is_some()
        {
            self.device = Some(device);
            return Ok(());
        }
        let engine = self.engine()?.clone();
        let (code, _) = engine.create_pairing_code().await?;
        let paired = engine
            .pair(&code, DEVICE_NAME, "test")
            .await
            .map_err(|e| anyhow::anyhow!("pairing the test device: {e}"))?;
        let device = Device {
            id: paired.device_id.to_string(),
            token: paired.token,
        };
        std::fs::write(
            self.paths.device_file(),
            serde_json::to_vec(&json!({"deviceId": device.id, "token": device.token}))?,
        )?;
        self.device = Some(device);
        Ok(())
    }

    async fn ready(&self) -> anyhow::Result<()> {
        let engine = self.engine()?;
        let device = self.device.clone().context("no paired device")?;
        let (code, _) = engine.create_pairing_code().await?;
        let project = self.root.join(NATIVE_PROJECT);
        let store = SessionStore::new(self.paths.native_sessions());
        let policy = self.policy.adapter_policy();
        let scan = {
            let project = project.clone();
            tokio::task::spawn_blocking(move || store.scan(&project, &policy))
                .await
                .context("listing the native sessions")?
                .context("listing the native sessions")?
        };
        if !scan.unreadable.is_empty() {
            bail!("unreadable native sessions: {:?}", scan.unreadable);
        }
        let sessions: Vec<Value> = scan
            .sessions
            .iter()
            .map(|s| json!({"nativeSessionId": s.native_session_id, "title": s.title}))
            .collect();
        emit(json!({
            "event": "ready",
            "wsUrl": self.ws_url()?,
            "httpUrl": self.http_url()?,
            "token": device.token,
            "deviceId": device.id,
            "pairingCode": code,
            "root": self.root.display().to_string(),
            "epoch": engine.epoch(),
            "nativeSessionsDir": self.paths.native_sessions().display().to_string(),
            "nativeProject": project.display().to_string(),
            "nativeSessions": sessions,
        }))
    }

    /// `native-session <folder> <prompt…>`: records a native session in `<root>/<folder>`.
    async fn native_session(&self, line: &str) -> anyhow::Result<()> {
        let rest = line
            .trim_start()
            .strip_prefix("native-session")
            .context("not a native-session command")?
            .trim_start();
        let (folder, prompt) = rest
            .split_once(char::is_whitespace)
            .context("usage: native-session <folder> <prompt…>")?;
        let prompt = prompt.trim().replace("\\n", "\n");
        if prompt.is_empty() {
            bail!("usage: native-session <folder> <prompt…>");
        }
        let relative = Path::new(folder);
        if !relative
            .components()
            .all(|c| matches!(c, std::path::Component::Normal(_)))
        {
            bail!("{folder} must be a folder under the project root");
        }
        let cwd = self.root.join(relative);
        std::fs::create_dir_all(&cwd).with_context(|| format!("creating {}", cwd.display()))?;
        let id = record(&self.paths.native_sessions(), &cwd, &[prompt]).await?;
        let store = SessionStore::new(self.paths.native_sessions());
        let transcript = tokio::task::spawn_blocking(move || store.read(&id))
            .await
            .context("reading the recorded session")?
            .context("reading the recorded session")?;
        emit(json!({
            "event": "nativeSession",
            "nativeSessionId": transcript.id,
            "cwd": cwd.display().to_string(),
            "title": transcript.title(&self.policy.adapter_policy()),
        }))
    }

    /// Runs one command line; `Ok(true)` asks the caller to quit.
    async fn command(&mut self, line: &str) -> anyhow::Result<bool> {
        let words: Vec<&str> = line.split_whitespace().collect();
        match words.as_slice() {
            ["chaos", "pass"] => self.proxy()?.set_mode(Mode::Pass),
            ["chaos", "drop"] => {
                let proxy = self.proxy()?;
                proxy.drop_all();
                proxy.set_mode(Mode::Pass);
            }
            ["chaos", "blackhole"] => self.proxy()?.set_mode(Mode::Blackhole),
            ["chaos", "delay", ms] => {
                let ms: u64 = ms.parse().context("chaos delay needs milliseconds")?;
                self.proxy()?
                    .set_mode(Mode::Delay(Duration::from_millis(ms)));
            }
            ["restart"] => {
                self.stop().await?;
                self.start().await?;
                self.ready().await?;
            }
            ["reset"] => {
                self.stop().await?;
                for file in self.paths.database_files() {
                    match std::fs::remove_file(&file) {
                        Ok(()) => {}
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                        Err(e) => {
                            return Err(e).with_context(|| format!("deleting {}", file.display()));
                        }
                    }
                }
                match std::fs::remove_file(self.paths.device_file()) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e).context("forgetting the test device"),
                }
                self.device = None;
                self.start().await?;
                self.ready().await?;
            }
            ["pairing-code"] => {
                let (code, _) = self.engine()?.create_pairing_code().await?;
                emit(json!({"event": "pairingCode", "code": code}))?;
            }
            ["native-session", ..] => self.native_session(line).await?,
            ["hold-session", id] => SessionStore::new(self.paths.native_sessions())
                .hold(id)
                .with_context(|| format!("holding session {id}"))?,
            ["release-session", id] => SessionStore::new(self.paths.native_sessions())
                .release(id)
                .with_context(|| format!("releasing session {id}"))?,
            ["quit"] => return Ok(true),
            _ => bail!("unknown command"),
        }
        Ok(false)
    }
}

/// Records a native session in `store`, bounded by [`RECORD_TIMEOUT`].
async fn record(store: &Path, cwd: &Path, prompts: &[String]) -> anyhow::Result<String> {
    tokio::time::timeout(RECORD_TIMEOUT, record_session(store, cwd, prompts))
        .await
        .with_context(|| format!("recording a session did not finish within {RECORD_TIMEOUT:?}"))?
        .context("recording a native session")
}

/// Records [`SEEDED_SESSIONS`] in `project` on the first start of a state dir (when the store
/// does not exist yet).
async fn seed_native_sessions(store: &Path, project: &Path) -> anyhow::Result<()> {
    if store.exists() {
        return Ok(());
    }
    std::fs::create_dir_all(project).with_context(|| format!("creating {}", project.display()))?;
    for turns in SEEDED_SESSIONS {
        let prompts: Vec<String> = turns.iter().map(|t| (*t).to_owned()).collect();
        record(store, project, &prompts).await?;
    }
    Ok(())
}

async fn run() -> anyhow::Result<()> {
    let args = parse_args()?;
    let mut server = TestServer::new(args).await?;
    server.start().await?;
    server.ready().await?;
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    loop {
        let Some(line) = lines.next_line().await.context("reading stdin")? else {
            break;
        };
        let line = line.trim().to_owned();
        if line.is_empty() {
            continue;
        }
        match server.command(&line).await {
            Ok(quit) => {
                emit(json!({"event": "ok", "cmd": line}))?;
                if quit {
                    break;
                }
            }
            Err(e) => emit(json!({"event": "error", "cmd": line, "message": format!("{e:#}")}))?,
        }
    }
    server.stop().await?;
    Ok(())
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("aas-test-server: cannot start the runtime: {e}");
            std::process::exit(1);
        }
    };
    let result = runtime.block_on(run());
    // Nothing may outlive the server: dropping the runtime ends every remaining task (their
    // process handles terminate what is left through the supervisor's jobs).
    drop(runtime);
    match result {
        Ok(()) => std::process::exit(0),
        Err(e) => {
            eprintln!("aas-test-server: {e:#}");
            std::process::exit(1);
        }
    }
}
