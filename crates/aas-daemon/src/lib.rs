//! The agent-app-server daemon: configuration, wiring (adapters are built here and injected
//! into the engine), logging, single-instance lock, stop signals, and the management CLI
//! helpers.

pub mod admin_client;
pub mod autostart;
pub mod config;
pub mod doctor;
pub mod endsession;
mod listener_owner;
pub mod policy;
pub mod power_plan;
pub mod qr;
pub mod signals;
pub mod tailscale;
pub mod watchdog;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use aas_core::{Engine, EngineConfig, HarnessRegistry};
use aas_harness::{AdapterContext, HarnessAdapter, HarnessConfig};
use aas_protocol::HarnessKind;
use aas_server::{Server, ServerOptions, ShutdownNotice, ShutdownReason};
use aas_supervisor::{PowerGuard, PowerLease, Supervisor, resolve_program};
use anyhow::{Context, anyhow};
use serde::Serialize;
use tokio::sync::{mpsc, oneshot, watch};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use crate::config::{Config, KeepAwake, Paths};
use crate::endsession::EndSession;
use crate::signals::{EndSessionSources, StopSignals, follow_signals};
pub use crate::signals::{StopLevel, follow_stop_requests};

// Exit codes of `agent-app-server run` and of the watchdog (design.md §18.2).

/// Stopped on request (`stop`, Ctrl+C, the end of the Windows session). The watchdog does not
/// restart the daemon and exits with this code too.
pub const EXIT_STOPPED: i32 = 0;
/// A failure while running (the transport died, the event log could not be written any
/// more, a panic exits with 101). The watchdog restarts the daemon after the backoff delay.
pub const EXIT_FAILURE: i32 = 1;
/// A configuration or startup error a retry cannot fix (invalid config.toml, `git.command`
/// not found, logging cannot be set up, an unusable data or config folder, an address that
/// cannot be bound for a reason other than "in use"). The watchdog stops instead of looping.
pub const EXIT_CONFIG: i32 = 2;
/// The watchdog found another watchdog running for the same data folder.
pub const EXIT_ALREADY_RUNNING: i32 = 3;
/// Something the daemon needs is held by someone else and may be released: a listen port is
/// in use, or another daemon holds `daemon.lock`. The watchdog retries with the backoff delay.
pub const EXIT_BUSY: i32 = 4;

/// Why the daemon could not run; decides its exit code.
#[derive(Debug)]
pub enum DaemonError {
    /// Fix the configuration first ([`EXIT_CONFIG`]).
    Config(anyhow::Error),
    /// Try again later ([`EXIT_BUSY`]).
    Busy(anyhow::Error),
    /// Failed while starting or running ([`EXIT_FAILURE`]).
    Failure(anyhow::Error),
}

impl DaemonError {
    pub fn exit_code(&self) -> i32 {
        match self {
            DaemonError::Config(_) => EXIT_CONFIG,
            DaemonError::Busy(_) => EXIT_BUSY,
            DaemonError::Failure(_) => EXIT_FAILURE,
        }
    }
}

impl std::fmt::Display for DaemonError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (DaemonError::Config(e) | DaemonError::Busy(e) | DaemonError::Failure(e)) = self;
        write!(f, "{e:#}")
    }
}

impl std::error::Error for DaemonError {}

/// Loads the configuration, writing the initial one on first run.
pub fn load_or_init_config(paths: &Paths) -> anyhow::Result<(Config, bool)> {
    let file = paths.config_file();
    if file.exists() {
        return Ok((Config::load(&file)?, false));
    }
    std::fs::create_dir_all(&paths.config_dir)
        .with_context(|| format!("creating {}", paths.config_dir.display()))?;
    let config = Config::initial();
    std::fs::write(&file, config.initial_text()?)
        .with_context(|| format!("writing {}", file.display()))?;
    Ok((config, true))
}

/// Logging: daily files under `logs/` (kept `log_retention_days`), plus stderr in the
/// foreground. `RUST_LOG` overrides the configured level.
pub fn init_logging(
    paths: &Paths,
    config: &Config,
    stderr: bool,
) -> anyhow::Result<tracing_appender::non_blocking::WorkerGuard> {
    std::fs::create_dir_all(paths.logs_dir())
        .with_context(|| format!("creating {}", paths.logs_dir().display()))?;
    let appender = tracing_appender::rolling::daily(paths.logs_dir(), "agent-app-server.log");
    let (file_writer, guard) = tracing_appender::non_blocking(appender);
    let filter = match EnvFilter::try_from_default_env() {
        Ok(filter) => filter,
        Err(_) => EnvFilter::try_new(&config.logging.level)
            .with_context(|| format!("logging.level {:?}", config.logging.level))?,
    };
    let file_layer = tracing_subscriber::fmt::layer()
        .with_writer(file_writer)
        .with_ansi(false);
    let registry = tracing_subscriber::registry().with(filter).with(file_layer);
    let installed = if stderr {
        registry
            .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
            .try_init()
    } else {
        registry.try_init()
    };
    installed.context("installing the log subscriber")?;
    prune_logs(paths, config.policy.log_retention_days);
    Ok(guard)
}

fn prune_logs(paths: &Paths, keep_days: u32) {
    let entries = match std::fs::read_dir(paths.logs_dir()) {
        Ok(entries) => entries,
        Err(e) => {
            tracing::warn!(error = %e, "cannot list the log folder; old logs are kept");
            return;
        }
    };
    let cutoff = std::time::SystemTime::now()
        - std::time::Duration::from_secs(u64::from(keep_days) * 24 * 3600);
    for e in entries.flatten() {
        let old = e
            .metadata()
            .and_then(|m| m.modified())
            .map(|m| m < cutoff)
            .unwrap_or(false);
        if old
            && e.file_name()
                .to_string_lossy()
                .starts_with("agent-app-server.log")
            && let Err(err) = std::fs::remove_file(e.path())
        {
            tracing::warn!(file = %e.path().display(), error = %err, "cannot remove an old log file");
        }
    }
}

/// Builds the adapters from configuration (dependency injection happens here and only here).
pub fn build_adapters(
    harnesses: &[HarnessConfig],
    supervisor: &Supervisor,
    config: &Config,
    data_dir: &std::path::Path,
) -> Vec<Arc<dyn HarnessAdapter>> {
    harnesses
        .iter()
        .map(|h| {
            let ctx = AdapterContext {
                supervisor: supervisor.clone(),
                state_dir: data_dir.join("adapters").join(&h.id),
                policy: config.policy.adapter_policy(),
            };
            let adapter: Arc<dyn HarnessAdapter> = match h.kind {
                HarnessKind::Codex => {
                    Arc::new(aas_adapter_codex::CodexAdapter::new(h.clone(), ctx))
                }
                HarnessKind::Claude => {
                    Arc::new(aas_adapter_claude::ClaudeAdapter::new(h.clone(), ctx))
                }
                HarnessKind::Pi => Arc::new(aas_adapter_pi::PiAdapter::new(h.clone(), ctx)),
                HarnessKind::Acp => Arc::new(aas_adapter_acp::AcpAdapter::new(h.clone(), ctx)),
                HarnessKind::Fake => Arc::new(aas_adapter_fake::FakeAdapter::new(h.clone(), ctx)),
            };
            adapter
        })
        .collect()
}

/// Holds an exclusive lock file (single instance) for as long as it lives.
pub struct InstanceLock {
    _file: std::fs::File,
}

/// Takes the exclusive lock on `path`; `None` when another process holds it. The OS releases
/// the lock when the holder exits, however it exits.
pub fn try_lock_file(path: &Path) -> anyhow::Result<Option<InstanceLock>> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(Some(InstanceLock { _file: file })),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(e)) => {
            Err(e).with_context(|| format!("locking {}", path.display()))
        }
    }
}

pub fn acquire_instance_lock(paths: &Paths) -> Result<InstanceLock, DaemonError> {
    match try_lock_file(&paths.lock_file()) {
        Ok(Some(lock)) => Ok(lock),
        Ok(None) => Err(DaemonError::Busy(anyhow!(
            "another agent-app-server daemon is already running for {}",
            paths.data_dir.display()
        ))),
        Err(e) => Err(DaemonError::Config(
            e.context("the data folder cannot be used"),
        )),
    }
}

/// Binds a listener. An address in use may be released later (retry); any other failure
/// (an address this PC does not have, a port Windows reserves) needs a configuration change.
async fn bind(addr: SocketAddr, key: &str) -> Result<tokio::net::TcpListener, DaemonError> {
    match tokio::net::TcpListener::bind(addr).await {
        Ok(listener) => Ok(listener),
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => Err(DaemonError::Busy(
            anyhow!(e).context(format!("{key} {addr} is in use by another program")),
        )),
        Err(e) => Err(DaemonError::Config(
            anyhow!(e).context(format!("{key} {addr} cannot be used")),
        )),
    }
}

/// The ready line written on stdout in background mode (read by the watchdog), and what the
/// daemon reports to an embedding caller.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Ready {
    pub event: &'static str,
    pub pid: u32,
    pub listen: SocketAddr,
    pub admin_listen: SocketAddr,
}

/// How `run_daemon` runs.
pub struct RunOptions {
    /// Started by the watchdog: write the ready line on stdout and follow its control lines
    /// on stdin.
    pub background: bool,
    /// Run by a supervisor that restarts the daemon after it exits with a failure (the
    /// watchdog, which starts it with `--background`). Clients are then told that a restart
    /// follows a stop for a failure (`server/shuttingDown.restartExpected`).
    pub supervised: bool,
    /// Receives the ready information once the daemon serves.
    pub on_ready: Option<oneshot::Sender<Ready>>,
    end_session: (
        mpsc::UnboundedSender<EndSession>,
        mpsc::UnboundedReceiver<EndSession>,
    ),
}

impl RunOptions {
    /// `background`: started by the watchdog (`run --background`), which also supervises it.
    pub fn new(background: bool) -> Self {
        Self {
            background,
            supervised: background,
            on_ready: None,
            end_session: mpsc::unbounded_channel(),
        }
    }

    /// Where end-session requests can be sent in addition to the window, the console and the
    /// watchdog's control line (tests).
    pub fn end_session_sender(&self) -> mpsc::UnboundedSender<EndSession> {
        self.end_session.0.clone()
    }
}

/// Follows the watchdog's control lines on stdin (background mode).
fn follow_control_lines(tx: mpsc::UnboundedSender<EndSession>, budget: std::time::Duration) {
    use tokio::io::AsyncBufReadExt;
    tokio::spawn(async move {
        let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => match line.trim() {
                    watchdog::END_SESSION_LINE => {
                        tracing::warn!("the watchdog reports that the session is ending");
                        if tx
                            .send(EndSession::unobserved(std::time::Instant::now() + budget))
                            .is_err()
                        {
                            return;
                        }
                    }
                    "" => {}
                    other => tracing::warn!(line = other, "unknown control line from the watchdog"),
                },
                // The watchdog closed the pipe: no more control lines (not a stop request).
                Ok(None) => return,
                Err(e) => {
                    tracing::warn!(error = %e, "reading control lines failed; the end-session window still applies");
                    return;
                }
            }
        }
    });
}

/// How the transport is closed and the engine stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stop {
    /// A drain finished: every agent was stopped while clients were still connected.
    Drained,
    Now,
    EndSession {
        deadline: tokio::time::Instant,
    },
    /// The engine fail-stopped (its event log cannot be written): exit with a failure so the
    /// watchdog restarts the daemon.
    StorageFailure,
}

/// What every client is told when the daemon stops this way (`server/shuttingDown`). A restart
/// follows only a stop for a failure (the event log could not be written, design.md §6.2) of
/// a daemon run by the watchdog; a requested stop, a drain and the end of the Windows session
/// end the watchdog too, and a daemon without a watchdog is restarted by nobody.
fn shutdown_notice(stop: Stop, supervised: bool) -> ShutdownNotice {
    let reason = match stop {
        Stop::Drained => ShutdownReason::Drain,
        Stop::StorageFailure => ShutdownReason::StorageFailure,
        Stop::Now | Stop::EndSession { .. } => ShutdownReason::Shutdown,
    };
    ShutdownNotice {
        reason,
        restart_expected: supervised && stop == Stop::StorageFailure,
    }
}

/// Waits for `fut`, but stops waiting at the end-session deadline once the session ends.
async fn bounded<F: Future>(
    level: &watch::Receiver<Option<StopLevel>>,
    what: &str,
    fut: F,
) -> Option<F::Output> {
    let mut level = level.clone();
    let deadline = async {
        let reached = level
            .wait_for(|l| matches!(l, Some(StopLevel::EndSession { .. })))
            .await
            .map(|l| *l);
        match reached {
            Ok(Some(StopLevel::EndSession { deadline })) => {
                tokio::time::sleep_until(deadline).await
            }
            _ => std::future::pending::<()>().await,
        }
    };
    tokio::select! {
        out = fut => Some(out),
        _ = deadline => {
            tracing::warn!(what, "the end-session deadline passed; the rest goes with the daemon's Job Objects");
            None
        }
    }
}

/// Stops every agent process, recording interrupted turns with the reason of the stop.
async fn stop_engine(
    engine: &Engine,
    level: &watch::Receiver<Option<StopLevel>>,
    end_session: bool,
) {
    if end_session {
        bounded(
            level,
            "stopping the agents",
            engine.shutdown_for_end_session(),
        )
        .await;
    } else {
        bounded(level, "stopping the agents", engine.shutdown(false)).await;
    }
}

/// The lease that keeps the PC awake for the daemon's whole life with `[power] keep_awake =
/// "always"` (design.md §4.6), or `None` with `"while_running"` (turns and busy background work
/// take their own leases). Leases are counted, so the daemon's adds to theirs.
fn keep_awake_lease(keep_awake: KeepAwake, power: &PowerGuard) -> Option<PowerLease> {
    match keep_awake {
        KeepAwake::Always => {
            tracing::info!(
                "keeping the PC awake while the daemon runs (power.keep_awake = \"always\")"
            );
            Some(power.acquire())
        }
        KeepAwake::WhileRunning => None,
    }
}

/// Runs the daemon until it is stopped. Returns the exit code for a stop that went as
/// requested; an error carries its own ([`DaemonError::exit_code`]).
pub async fn run_daemon(
    paths: Paths,
    config: Config,
    options: RunOptions,
) -> Result<i32, DaemonError> {
    let _lock = acquire_instance_lock(&paths)?;
    let admin_token = config::admin_token(&paths).map_err(DaemonError::Config)?;
    let git = match &config.git.command {
        Some(cmd) => Some(
            resolve_program(cmd)
                .with_context(|| format!("git.command {cmd}"))
                .map_err(DaemonError::Config)?,
        ),
        None => resolve_program("git").ok(),
    };
    if git.is_none() {
        tracing::warn!("git was not found: diffs, worktrees and clone are disabled");
    }
    // Bound before the engine starts, so that a daemon that cannot serve does not run the
    // startup recovery for nothing.
    let listener = bind(config.server.listen, "server.listen").await?;
    let admin_listener = bind(config.server.admin_listen, "server.admin_listen").await?;
    let listen = listener
        .local_addr()
        .map_err(|e| DaemonError::Failure(e.into()))?;
    let admin_listen = admin_listener
        .local_addr()
        .map_err(|e| DaemonError::Failure(e.into()))?;
    let policy = config.policy.clone();
    let supervisor = Supervisor::new(
        &paths.data_dir.join("supervisor"),
        policy.supervisor_policy(),
    )
    .with_context(|| {
        format!(
            "the data folder {} cannot be used",
            paths.data_dir.display()
        )
    })
    .map_err(DaemonError::Config)?;
    let sweep = supervisor.sweep_orphans();
    if !sweep.terminated.is_empty() || !sweep.failed.is_empty() {
        tracing::warn!(
            terminated = sweep.terminated.len(),
            failed = sweep.failed.len(),
            "processes left over from a previous run were cleaned up"
        );
    }
    let _awake = keep_awake_lease(config.power.keep_awake, supervisor.power());
    let adapters = build_adapters(&config.harnesses, &supervisor, &config, &paths.data_dir);
    let engine = Engine::start(
        EngineConfig {
            data_dir: paths.data_dir.clone(),
            server_name: config.server.name.clone().unwrap_or_else(config::hostname),
            hostname: config::hostname(),
            project_roots: config.projects.roots.clone(),
            policy,
            heuristics: config.heuristics.clone(),
            git,
        },
        HarnessRegistry::new(adapters),
        supervisor,
    )
    .await
    .context("starting the engine")
    .map_err(DaemonError::Failure)?;

    let server = Server::with_policy(
        engine.clone(),
        ServerOptions {
            listen,
            public_url: config.server.public_url.clone(),
            admin_token,
        },
        config.server_policy.clone(),
    );

    // Stop sources.
    let budget = config.daemon_policy.end_session_deadline;
    let RunOptions {
        background,
        supervised,
        on_ready,
        end_session: (end_tx, end_rx),
    } = options;
    #[cfg(windows)]
    let _window = match endsession::EndSessionWindow::start(budget, end_tx.clone()) {
        Ok(window) => Some(window),
        Err(e) => {
            tracing::error!(error = %e, "cannot listen for the end of the Windows session");
            None
        }
    };
    if background {
        follow_control_lines(end_tx.clone(), budget);
    }
    drop(end_tx);
    let signals: StopSignals = follow_signals(
        server.stop_requests(),
        EndSessionSources {
            requests: end_rx,
            budget,
            console: true,
        },
    );
    let level = signals.level();

    let (admin_stop_tx, admin_stop_rx) = oneshot::channel::<()>();
    let admin_serving = server
        .serve_admin(admin_listener, async move {
            let _ = admin_stop_rx.await;
        })
        .context("serving the admin API")
        .map_err(DaemonError::Config)?;
    let admin_serving = tokio::spawn(admin_serving);
    let (notice_tx, notice_rx) = oneshot::channel::<ShutdownNotice>();
    let mut serving = tokio::spawn(server.run_until(listener, async move {
        // Without a decided notice the daemon is failing (it returns an error, and the
        // watchdog, if there is one, restarts it).
        notice_rx.await.unwrap_or(ShutdownNotice {
            reason: ShutdownReason::Shutdown,
            restart_expected: supervised,
        })
    }));
    tracing::info!(%listen, %admin_listen, public_url = ?config.server.public_url, "agent-app-server is running");
    let ready = Ready {
        event: "ready",
        pid: std::process::id(),
        listen,
        admin_listen,
    };
    if background {
        use std::io::Write;
        let line = serde_json::to_string(&ready).expect("the ready line serializes");
        let mut out = std::io::stdout().lock();
        if let Err(e) = writeln!(out, "{line}").and_then(|()| out.flush()) {
            tracing::error!(error = %e, "could not write the ready line for the watchdog");
        }
    }
    if let Some(tx) = on_ready {
        let _ = tx.send(ready);
    }

    // 1. The first reason to stop.
    let mut watch_level = level.clone();
    let first = tokio::select! {
        l = async { watch_level.wait_for(Option::is_some).await.map(|l| *l) } => match l {
            Ok(Some(StopLevel::Drain)) => None,
            Ok(Some(StopLevel::EndSession { deadline })) => Some(Stop::EndSession { deadline }),
            // Now, or the signal follower is gone (the runtime is ending).
            _ => Some(Stop::Now),
        },
        failure = engine.wait_storage_failure() => {
            tracing::error!(error = %failure.message, "stopping after a storage failure");
            Some(Stop::StorageFailure)
        }
        ended = &mut serving => {
            let why = match ended {
                Ok(Ok(())) => anyhow!("the transport stopped by itself"),
                Ok(Err(e)) => anyhow!(e).context("serving failed"),
                Err(e) => anyhow!(e).context("the transport task failed"),
            };
            tracing::error!(error = %format!("{why:#}"), "stopping");
            stop_engine(&engine, &level, false).await;
            let _ = admin_stop_tx.send(());
            return Err(DaemonError::Failure(why));
        }
    };
    // 2. A drain waits for the running turns and the background work that keeps an agent
    //    busy (`Engine::wait_drained`), then stops every agent while clients are still
    //    connected; a later request can cut it short.
    let stop = match first {
        Some(stop) => stop,
        None => {
            tracing::info!(
                "draining: waiting for running turns and busy background work to finish"
            );
            let mut escalation = level.clone();
            tokio::select! {
                _ = engine.wait_drained() => {
                    stop_engine(&engine, &level, false).await;
                    Stop::Drained
                }
                l = async { escalation.wait_for(|l| !matches!(l, Some(StopLevel::Drain))).await.map(|l| *l) } => match l {
                    Ok(Some(StopLevel::EndSession { deadline })) => Stop::EndSession { deadline },
                    _ => {
                        tracing::info!("stop requested while draining: stopping now");
                        Stop::Now
                    }
                },
                _ = engine.wait_storage_failure() => Stop::StorageFailure,
            }
        }
    };
    // 3. Tell every client the same reason and close the transport; stop the agents.
    let _ = notice_tx.send(shutdown_notice(stop, supervised));
    let transport_closed = async {
        match (&mut serving).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::error!(error = %e, "the transport failed while closing"),
            Err(e) => tracing::error!(error = %e, "the transport task failed while closing"),
        }
    };
    // Closing the transport is bounded by `policy.transport_shutdown_timeout` (a client that
    // stopped reading cannot hold it).
    match stop {
        Stop::Drained => {
            bounded(&level, "closing connections", transport_closed).await;
        }
        Stop::Now | Stop::StorageFailure => {
            // The agents stop while the clients are told and disconnected: stopping them does
            // not wait for the slowest client.
            tokio::join!(
                bounded(&level, "closing connections", transport_closed),
                stop_engine(&engine, &level, false)
            );
        }
        Stop::EndSession { deadline } => {
            // Windows ends the session: everything within the deadline, agents and clients at
            // the same time.
            let both = async { tokio::join!(transport_closed, engine.shutdown_for_end_session()) };
            if tokio::time::timeout_at(deadline, both).await.is_err() {
                tracing::warn!(
                    "the end-session deadline passed; the rest goes with the daemon's Job Objects"
                );
            }
        }
    }
    let _ = admin_stop_tx.send(());
    match bounded(&level, "closing the admin listener", admin_serving).await {
        Some(Ok(Ok(()))) | None => {}
        Some(Ok(Err(e))) => tracing::error!(error = %e, "the admin listener failed"),
        Some(Err(e)) => tracing::error!(error = %e, "the admin listener task failed"),
    }
    signals.acknowledge_end_session();
    match stop {
        Stop::StorageFailure => {
            let message = engine
                .storage_failure()
                .map(|f| f.message)
                .unwrap_or_default();
            Err(DaemonError::Failure(anyhow!(
                "stopped after a storage failure: {message}"
            )))
        }
        _ => {
            tracing::info!("agent-app-server stopped");
            Ok(EXIT_STOPPED)
        }
    }
}

/// Folder of the running executable (the daemon and CLI are installed side by side).
pub fn exe_dir() -> anyhow::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    Ok(exe
        .parent()
        .context("executable has no parent folder")?
        .to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_files_admit_one_holder_at_a_time() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("watchdog.lock");
        let first = try_lock_file(&path)
            .unwrap()
            .expect("the first holder gets the lock");
        assert!(
            try_lock_file(&path).unwrap().is_none(),
            "a second holder is refused"
        );
        drop(first);
        assert!(
            try_lock_file(&path).unwrap().is_some(),
            "the lock is free again once released"
        );
    }

    #[test]
    fn clients_are_told_of_a_restart_only_when_the_watchdog_will_restart_the_daemon() {
        let deadline = tokio::time::Instant::now();
        let cases = [
            (Stop::Now, ShutdownReason::Shutdown),
            (Stop::Drained, ShutdownReason::Drain),
            (Stop::EndSession { deadline }, ShutdownReason::Shutdown),
            (Stop::StorageFailure, ShutdownReason::StorageFailure),
        ];
        for (stop, reason) in cases {
            for supervised in [false, true] {
                let notice = shutdown_notice(stop, supervised);
                assert_eq!(notice.reason, reason);
                assert_eq!(
                    notice.restart_expected,
                    supervised && stop == Stop::StorageFailure,
                    "{stop:?} supervised={supervised}"
                );
            }
        }
    }

    /// `keep_awake = "always"` holds one lease for as long as the daemon keeps it (the whole
    /// run); turns and background work add and drop theirs on top. `"while_running"` holds none.
    /// The guard is a counter here (no request reaches the OS).
    #[test]
    fn keep_awake_always_holds_one_lease_for_the_daemons_life() {
        let power = PowerGuard::new(false);
        let daemon = keep_awake_lease(KeepAwake::Always, &power);
        assert!(daemon.is_some());
        assert_eq!(power.active(), 1);
        let turn = power.acquire();
        let background = power.acquire();
        assert_eq!(power.active(), 3);
        drop(turn);
        drop(background);
        assert_eq!(
            power.active(),
            1,
            "the daemon's lease outlives turns and work"
        );
        drop(daemon);
        assert_eq!(power.active(), 0);

        assert!(keep_awake_lease(KeepAwake::WhileRunning, &power).is_none());
        assert_eq!(power.active(), 0);
    }

    #[test]
    fn exit_codes_follow_the_error_class() {
        assert_eq!(DaemonError::Config(anyhow!("x")).exit_code(), EXIT_CONFIG);
        assert_eq!(DaemonError::Busy(anyhow!("x")).exit_code(), EXIT_BUSY);
        assert_eq!(DaemonError::Failure(anyhow!("x")).exit_code(), EXIT_FAILURE);
    }

    #[tokio::test]
    async fn a_port_in_use_is_busy_and_a_foreign_address_is_a_configuration_error() {
        let taken = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let err = bind(taken.local_addr().unwrap(), "server.listen")
            .await
            .unwrap_err();
        assert_eq!(err.exit_code(), EXIT_BUSY, "{err}");
        // TEST-NET-1 (RFC 5737) is never assigned to this machine.
        let err = bind("192.0.2.1:7878".parse().unwrap(), "server.listen")
            .await
            .unwrap_err();
        assert_eq!(err.exit_code(), EXIT_CONFIG, "{err}");
    }
}

#[cfg(test)]
mod stop_tests {
    use super::*;
    use std::time::Duration;

    use aas_protocol::methods::*;

    /// A config for an in-process daemon on free loopback ports with the in-process fake
    /// harness.
    fn test_config(root: &Path) -> Config {
        let mut config = Config::default();
        config.server.listen = "127.0.0.1:0".parse().unwrap();
        config.server.admin_listen = "127.0.0.1:0".parse().unwrap();
        config.server.public_url = Some("ws://127.0.0.1:1/v1/ws".into());
        config.projects.roots = vec![root.to_path_buf()];
        config.policy.stop_grace = Duration::from_millis(300);
        config.policy.prevent_sleep_while_running = false;
        config.harnesses = vec![HarnessConfig {
            id: "fake".into(),
            kind: HarnessKind::Fake,
            display_name: None,
            command: String::new(),
            args: Vec::new(),
            env: Default::default(),
            options: serde_json::json!({"mode": "inProcess"}),
        }];
        config
    }

    struct Running {
        _dir: tempfile::TempDir,
        paths: Paths,
        root: PathBuf,
        ready: Ready,
        end_session: mpsc::UnboundedSender<EndSession>,
        daemon: tokio::task::JoinHandle<Result<i32, DaemonError>>,
    }

    async fn start_daemon() -> Running {
        start_daemon_with(|_| {}).await
    }

    async fn start_daemon_with(adjust: impl FnOnce(&mut Config)) -> Running {
        start_daemon_as(adjust, false).await
    }

    /// `supervised`: as if run by the watchdog.
    async fn start_daemon_as(adjust: impl FnOnce(&mut Config), supervised: bool) -> Running {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_dir: dir.path().join("cfg"),
            data_dir: dir.path().join("data"),
        };
        let root = dunce::canonicalize({
            let r = dir.path().join("projects");
            std::fs::create_dir_all(r.join("p")).unwrap();
            r
        })
        .unwrap();
        let mut config = test_config(&root);
        adjust(&mut config);
        let mut options = RunOptions::new(false);
        options.supervised = supervised;
        let (ready_tx, ready_rx) = oneshot::channel();
        options.on_ready = Some(ready_tx);
        let end_session = options.end_session_sender();
        let daemon = tokio::spawn(run_daemon(paths.clone(), config, options));
        let ready = tokio::time::timeout(Duration::from_secs(30), ready_rx)
            .await
            .expect("the daemon starts")
            .unwrap();
        Running {
            _dir: dir,
            paths,
            root,
            ready,
            end_session,
            daemon,
        }
    }

    fn admin(r: &Running) -> admin_client::AdminClient {
        admin_client::AdminClient::new(
            r.ready.admin_listen,
            Some(config::admin_token(&r.paths).unwrap()),
            admin_client::Timeouts {
                connect: Duration::from_secs(5),
                request: Duration::from_secs(30),
            },
        )
    }

    /// Opens the database read-only and returns the error kinds of the thread's turns.
    fn turn_errors(paths: &Paths) -> Vec<Option<String>> {
        let db = rusqlite::Connection::open_with_flags(
            paths.data_dir.join("aas.db"),
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
                    serde_json::from_str::<serde_json::Value>(&json).unwrap()["kind"]
                        .as_str()
                        .unwrap()
                        .to_owned()
                })
            })
            .collect()
    }

    /// Pairs a device through the admin API and the public endpoint; returns its token.
    async fn pair_device(r: &Running) -> String {
        let code: aas_protocol::http::AdminPairingCodeResponse = admin(r)
            .post(
                "/v1/admin/pairing-codes",
                &aas_protocol::http::AdminPairingCodeRequest {},
            )
            .await
            .unwrap();
        let body = serde_json::json!({"code": code.code, "deviceName": "test", "platform": "test"});
        let paired = pair_over_http(r.ready.listen, &body).await;
        paired["token"].as_str().unwrap().to_owned()
    }

    /// Starts a turn that runs far longer than the test, through the public WebSocket.
    async fn start_long_turn(r: &Running) {
        let token = pair_device(r).await;
        let mut ws = crate::stop_tests::ws::Ws::connect(r.ready.listen, &token).await;
        ws.call("initialize", serde_json::json!({"protocolVersion": 1, "client": {"name": "t", "version": "1", "platform": "t"}})).await;
        let project: ProjectResult = serde_json::from_value(
            ws.call("project/open", serde_json::json!({"clientRequestId": "p", "path": r.root.join("p").display().to_string()})).await,
        )
        .unwrap();
        ws.call(
            "thread/create",
            serde_json::json!({"clientRequestId": "t", "projectId": project.project.id, "harnessId": "fake",
                "input": [{"type": "text", "text": "@sleep 600000"}]}),
        )
        .await;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        loop {
            let status: aas_protocol::http::AdminStatusResponse =
                admin(r).get("/v1/admin/status").await.unwrap();
            if status.running_turns > 0 {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the turn never started"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn pair_over_http(addr: SocketAddr, body: &serde_json::Value) -> serde_json::Value {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let body = serde_json::to_vec(body).unwrap();
        let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
        let head = format!(
            "POST /v1/pair HTTP/1.1\r\nHost: x\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        s.write_all(head.as_bytes()).await.unwrap();
        s.write_all(&body).await.unwrap();
        let mut buf = Vec::new();
        s.read_to_end(&mut buf).await.unwrap();
        let split = buf.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
        serde_json::from_slice(&buf[split + 4..]).unwrap()
    }

    mod ws {
        use futures::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;

        pub struct Ws {
            inner: tokio_tungstenite::WebSocketStream<
                tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
            >,
            next: i64,
            pub notifications: Vec<serde_json::Value>,
        }

        impl Ws {
            pub async fn connect(addr: std::net::SocketAddr, token: &str) -> Self {
                let mut req = format!("ws://{addr}/v1/ws").into_client_request().unwrap();
                req.headers_mut()
                    .insert("Authorization", format!("Bearer {token}").parse().unwrap());
                let (inner, _) = tokio_tungstenite::connect_async(req).await.unwrap();
                Self {
                    inner,
                    next: 1,
                    notifications: Vec::new(),
                }
            }

            /// Reads until the server closes the connection; returns the parameters of the
            /// `server/shuttingDown` it sent before.
            pub async fn shutdown_notice(&mut self) -> serde_json::Value {
                let mut notice = None;
                while let Some(Ok(message)) = self.inner.next().await {
                    if let Message::Text(t) = message {
                        let v: serde_json::Value = serde_json::from_str(&t).unwrap();
                        if v["method"] == "server/shuttingDown" {
                            notice = Some(v["params"].clone());
                        }
                    }
                }
                notice.expect("server/shuttingDown before the close")
            }

            pub async fn call(
                &mut self,
                method: &str,
                params: serde_json::Value,
            ) -> serde_json::Value {
                let id = self.next;
                self.next += 1;
                let msg = serde_json::json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
                self.inner
                    .send(Message::text(msg.to_string()))
                    .await
                    .unwrap();
                loop {
                    let Some(Ok(Message::Text(t))) = self.inner.next().await else {
                        panic!("connection closed during {method}")
                    };
                    let v: serde_json::Value = serde_json::from_str(&t).unwrap();
                    if v["id"] == id {
                        assert!(v.get("error").is_none(), "{method}: {v}");
                        return v["result"].clone();
                    }
                    self.notifications.push(v);
                }
            }
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_stop_after_a_drain_ends_the_drain() {
        let r = start_daemon().await;
        start_long_turn(&r).await;
        admin(&r)
            .post_empty("/v1/admin/stop", serde_json::json!({"drain": true}))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            !r.daemon.is_finished(),
            "the drain waits for the running turn"
        );
        let status: aas_protocol::http::AdminStatusResponse =
            admin(&r).get("/v1/admin/status").await.unwrap();
        assert!(status.draining);
        // The follow-up request is still heard and ends the wait.
        admin(&r)
            .post_empty("/v1/admin/stop", serde_json::json!({"drain": false}))
            .await
            .unwrap();
        let code = tokio::time::timeout(Duration::from_secs(20), r.daemon)
            .await
            .expect("the escalated stop proceeds")
            .unwrap()
            .unwrap();
        assert_eq!(code, EXIT_STOPPED);
        assert_eq!(
            turn_errors(&r.paths),
            vec![Some("daemonShutdown".to_owned())]
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_requested_stop_tells_clients_that_nothing_restarts_the_daemon() {
        // Also under the watchdog: it exits after a requested stop.
        for supervised in [false, true] {
            let r = start_daemon_as(|_| {}, supervised).await;
            let token = pair_device(&r).await;
            let mut ws = crate::stop_tests::ws::Ws::connect(r.ready.listen, &token).await;
            ws.call("initialize", serde_json::json!({"protocolVersion": 1, "client": {"name": "t", "version": "1", "platform": "t"}})).await;
            admin(&r)
                .post_empty("/v1/admin/stop", serde_json::json!({"drain": false}))
                .await
                .unwrap();
            let notice = tokio::time::timeout(Duration::from_secs(20), ws.shutdown_notice())
                .await
                .expect("the client is told");
            assert_eq!(notice["reason"], "shutdown");
            assert_eq!(notice["restartExpected"], false, "supervised={supervised}");
            let code = tokio::time::timeout(Duration::from_secs(20), r.daemon)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert_eq!(code, EXIT_STOPPED);
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_download_nobody_reads_does_not_hold_the_stop() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let limit = Duration::from_secs(1);
        let r = start_daemon_with(|c| {
            c.server_policy.writer_flush_timeout = Duration::from_millis(500);
            c.server_policy.transport_shutdown_timeout = limit;
        })
        .await;
        start_long_turn(&r).await;
        let token = pair_device(&r).await;
        // A large image, uploaded...
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        png.resize(20 * 1024 * 1024, 0x55);
        let mut upload = tokio::net::TcpStream::connect(r.ready.listen)
            .await
            .unwrap();
        let head = format!(
            "POST /v1/blobs HTTP/1.1\r\nHost: x\r\nConnection: close\r\nAuthorization: Bearer {token}\r\nContent-Type: image/png\r\nContent-Length: {}\r\n\r\n",
            png.len()
        );
        upload.write_all(head.as_bytes()).await.unwrap();
        upload.write_all(&png).await.unwrap();
        let mut answer = Vec::new();
        upload.read_to_end(&mut answer).await.unwrap();
        let split = answer.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
        let blob: serde_json::Value = serde_json::from_slice(&answer[split + 4..]).unwrap();
        // ...and downloaded by a phone that froze the app while its connection stays open: it
        // reads nothing, with a tiny receive window.
        let socket = tokio::net::TcpSocket::new_v4().unwrap();
        socket.set_recv_buffer_size(4096).unwrap();
        let mut stalled = socket.connect(r.ready.listen).await.unwrap();
        let get = format!(
            "GET /v1/blobs/{} HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {token}\r\n\r\n",
            blob["blobId"].as_str().unwrap()
        );
        stalled.write_all(get.as_bytes()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;
        admin(&r)
            .post_empty("/v1/admin/stop", serde_json::json!({"drain": false}))
            .await
            .unwrap();
        let code = tokio::time::timeout(Duration::from_secs(30), r.daemon)
            .await
            .expect("the stop is not held by the download")
            .unwrap()
            .unwrap();
        assert_eq!(code, EXIT_STOPPED);
        assert_eq!(
            turn_errors(&r.paths),
            vec![Some("daemonShutdown".to_owned())],
            "the agents were stopped"
        );
        drop(stalled);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_end_of_the_session_stops_within_the_deadline_with_its_own_reason() {
        let r = start_daemon().await;
        start_long_turn(&r).await;
        let deadline = std::time::Instant::now() + Duration::from_secs(4);
        let (request, waiter) = EndSession::with_waiter(deadline);
        r.end_session.send(request).unwrap();
        let waiting = tokio::task::spawn_blocking(move || waiter.wait_until(deadline));
        let code = tokio::time::timeout(Duration::from_secs(10), r.daemon)
            .await
            .expect("the daemon stops")
            .unwrap()
            .unwrap();
        assert_eq!(code, EXIT_STOPPED);
        assert!(
            std::time::Instant::now() <= deadline + Duration::from_secs(1),
            "within the end-session deadline"
        );
        assert!(
            waiting.await.unwrap(),
            "the window procedure is released when the shutdown is done"
        );
        assert_eq!(
            turn_errors(&r.paths),
            vec![Some("systemShutdown".to_owned())]
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_admin_api_is_only_on_the_admin_listener() {
        let r = start_daemon().await;
        let wrong = admin_client::AdminClient::new(
            r.ready.listen,
            Some(config::admin_token(&r.paths).unwrap()),
            admin_client::Timeouts {
                connect: Duration::from_secs(5),
                request: Duration::from_secs(5),
            },
        );
        let err = wrong
            .get::<aas_protocol::http::AdminStatusResponse>("/v1/admin/status")
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("404"), "{err:#}");
        admin(&r).liveness().await.unwrap();
        admin(&r)
            .post_empty("/v1/admin/stop", serde_json::json!({"drain": false}))
            .await
            .unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(20), r.daemon)
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
            EXIT_STOPPED
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_second_daemon_on_the_same_data_folder_is_busy() {
        let r = start_daemon().await;
        let config = test_config(&r.root);
        let err = run_daemon(r.paths.clone(), config, RunOptions::new(false))
            .await
            .unwrap_err();
        assert_eq!(err.exit_code(), EXIT_BUSY, "{err}");
        admin(&r)
            .post_empty("/v1/admin/stop", serde_json::json!({"drain": false}))
            .await
            .unwrap();
        r.daemon.await.unwrap().unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_unknown_git_command_is_a_configuration_error() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_dir: dir.path().join("cfg"),
            data_dir: dir.path().join("data"),
        };
        let mut config = test_config(dir.path());
        config.git.command = Some("definitely-not-a-git-binary-aas".into());
        let err = run_daemon(paths, config, RunOptions::new(false))
            .await
            .unwrap_err();
        assert_eq!(err.exit_code(), EXIT_CONFIG, "{err}");
    }
}
