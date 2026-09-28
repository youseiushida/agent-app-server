//! Watchdog (`agent-app-server-daemon`): runs `agent-app-server run --background` as a
//! supervised child and restarts it when it exits unexpectedly or stops being live.
//!
//! The daemon runs inside the watchdog's Job Object, so ending the watchdog also ends the
//! daemon (and, through the daemon's own jobs, every agent) — nothing is left running.
//!
//! Contract with the daemon (design.md §18.2):
//! * exit codes: 0 = stopped on request (the watchdog exits too), 2 = configuration or
//!   startup error a retry cannot fix (the watchdog exits with 2), anything else = restart
//!   after the backoff delay;
//! * the daemon writes one ready line on stdout once it serves
//!   (`{"event":"ready","pid":…,"listen":…,"adminListen":…}`);
//! * from then on the watchdog checks `GET /v1/liveness` on the admin listener every
//!   `policy.liveness_interval`; after `policy.liveness_failures` consecutive failures (no
//!   answer within `policy.liveness_timeout`, or not 200) it kills the daemon's process tree
//!   and restarts it. A daemon that never reports ready within `policy.watchdog_ready_timeout`
//!   is handled the same way. Both are explicit protocol signals with policy values, like the
//!   client heartbeats — nothing is inferred from silence in the daemon's output;
//! * when Windows ends the session the watchdog writes `end-session` on the daemon's stdin,
//!   waits for it to exit (at most `policy.end_session_deadline`), never restarts it, and
//!   exits with 0.
//!
//! Contract with Task Scheduler (design.md §18.3): the logon task starts the watchdog
//! explicitly; the keep-alive task starts it every few minutes with `--keepalive`, because
//! Task Scheduler does not restart a program that exits with a failure. When the watchdog
//! ends on purpose — the daemon was stopped on request, or cannot start with this
//! configuration — it records that in `watchdog-stopped.json`; a keep-alive start then exits
//! at once, and the next explicit start removes the record.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::pin::Pin;
use std::time::Duration;

use aas_supervisor::{ChildHandle, ExitInfo, SpawnSpec, StopReason, Supervisor};
use anyhow::Context;
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::admin_client::{AdminClient, Timeouts};
use crate::config::{Config, Paths};
use crate::endsession::EndSession;
use crate::policy::DaemonPolicy;
use crate::{
    EXIT_ALREADY_RUNNING, EXIT_CONFIG, EXIT_FAILURE, EXIT_STOPPED, InstanceLock, exe_dir,
    try_lock_file,
};

/// The control line that passes the end of the session on to the daemon.
pub const END_SESSION_LINE: &str = "end-session";

/// Doubling restart delay, reset after a stable run.
#[derive(Debug, Clone)]
pub struct Backoff {
    min: Duration,
    max: Duration,
    stable_run: Duration,
    next: Duration,
}

impl Backoff {
    pub fn new(min: Duration, max: Duration, stable_run: Duration) -> Self {
        Self {
            min,
            max,
            stable_run,
            next: min,
        }
    }

    /// The delay before the next start, after a run that lasted `ran`.
    pub fn after_run(&mut self, ran: Duration) -> Duration {
        if ran >= self.stable_run {
            self.next = self.min;
        }
        let delay = self.next;
        self.next = (self.next * 2).min(self.max);
        delay
    }
}

/// Something that happened to the supervised daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChildEvent {
    /// The ready line: the daemon serves, its admin listener is `admin`.
    Ready { admin: SocketAddr },
    /// The daemon's process tree is gone.
    Exited(ExitInfo),
}

/// Control of a started daemon.
pub trait ChildControl: Send + Sync {
    fn pid(&self) -> u32;
    /// Terminates the daemon's whole process tree; the exit arrives as [`ChildEvent::Exited`].
    fn kill(&self);
    /// Passes the end of the session on to the daemon.
    fn end_session(&self);
}

pub struct Launched {
    pub control: Box<dyn ChildControl>,
    pub events: mpsc::UnboundedReceiver<ChildEvent>,
}

/// Starts the daemon.
pub trait Launcher: Send {
    fn launch(&mut self) -> impl Future<Output = anyhow::Result<Launched>> + Send;
}

pub type CheckFuture = Pin<Box<dyn Future<Output = Result<(), String>> + Send>>;

/// One liveness check against the admin listener.
pub trait Probe: Send + Sync {
    fn check(&self, admin: SocketAddr) -> CheckFuture;
}

/// Where the watchdog writes what it does (`logs\watchdog.log`).
pub trait Log: Send + Sync {
    fn line(&self, line: &str);
}

/// The watchdog's policy values (from `[policy]`, see [`DaemonPolicy`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchdogPolicy {
    pub restart_delay_min: Duration,
    pub restart_delay_max: Duration,
    pub stable_run: Duration,
    pub ready_timeout: Duration,
    pub liveness_interval: Duration,
    pub liveness_timeout: Duration,
    pub liveness_failures: u32,
}

impl From<&DaemonPolicy> for WatchdogPolicy {
    fn from(p: &DaemonPolicy) -> Self {
        Self {
            restart_delay_min: p.watchdog_restart_delay_min,
            restart_delay_max: p.watchdog_restart_delay_max,
            stable_run: p.watchdog_stable_run,
            ready_timeout: p.watchdog_ready_timeout,
            liveness_interval: p.liveness_interval,
            liveness_timeout: p.liveness_timeout,
            liveness_failures: p.liveness_failures,
        }
    }
}

/// Why the watchdog ended (it restarts the daemon in every other case).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ended {
    /// The daemon was stopped on request (`stop`, Ctrl+C in its console).
    Stopped,
    /// Windows ends the session.
    SessionEnded,
    /// The daemon cannot start with this configuration; a retry cannot fix it.
    ConfigError,
}

impl Ended {
    pub fn exit_code(self) -> i32 {
        match self {
            Ended::Stopped | Ended::SessionEnded => EXIT_STOPPED,
            Ended::ConfigError => EXIT_CONFIG,
        }
    }

    /// Whether the keep-alive task must not start the watchdog again until an explicit start:
    /// it ended on purpose. The end of the session is not recorded (the next logon starts it
    /// explicitly anyway).
    pub fn is_deliberate(self) -> bool {
        matches!(self, Ended::Stopped | Ended::ConfigError)
    }
}

/// What one run of the daemon leads to.
enum Outcome {
    Restart,
    Exit(Ended),
}

/// Runs the daemon until it stops for good. Returns why.
pub async fn supervise(
    launcher: &mut impl Launcher,
    probe: &dyn Probe,
    policy: &WatchdogPolicy,
    log: &dyn Log,
    mut end_session: mpsc::UnboundedReceiver<EndSession>,
) -> Ended {
    let mut backoff = Backoff::new(
        policy.restart_delay_min,
        policy.restart_delay_max,
        policy.stable_run,
    );
    // End-session requests are acknowledged only when the watchdog is about to exit, so that
    // the window procedure keeps Windows from terminating it (and with it the daemon's job)
    // while the daemon shuts down.
    let mut ending: Vec<EndSession> = Vec::new();
    loop {
        let started = Instant::now();
        let outcome = match launcher.launch().await {
            Ok(launched) => {
                run_once(launched, probe, policy, log, &mut end_session, &mut ending).await
            }
            Err(e) => {
                log.line(&format!("could not start the daemon: {e:#}"));
                if ending.is_empty() {
                    Outcome::Restart
                } else {
                    Outcome::Exit(Ended::SessionEnded)
                }
            }
        };
        if let Outcome::Exit(ended) = outcome {
            for request in ending.drain(..) {
                request.ack();
            }
            return ended;
        }
        let delay = backoff.after_run(started.elapsed());
        log.line(&format!("restarting the daemon in {delay:?}"));
        tokio::select! {
            _ = tokio::time::sleep(delay) => {}
            Some(request) = end_session.recv() => {
                log.line("the session is ending; the daemon is not restarted");
                request.ack();
                return Ended::SessionEnded;
            }
        }
    }
}

async fn run_once(
    launched: Launched,
    probe: &dyn Probe,
    policy: &WatchdogPolicy,
    log: &dyn Log,
    end_session: &mut mpsc::UnboundedReceiver<EndSession>,
    ending: &mut Vec<EndSession>,
) -> Outcome {
    let Launched {
        control,
        mut events,
    } = launched;
    log.line(&format!("daemon started (pid {})", control.pid()));
    let ready_by = Instant::now() + policy.ready_timeout;
    let mut admin: Option<SocketAddr> = None;
    let mut next_check = ready_by;
    let mut check: Option<CheckFuture> = None;
    let mut failures = 0u32;
    let mut killed: Option<&'static str> = None;
    let mut end_by: Option<Instant> = None;
    let mut end_open = true;
    loop {
        let quiet = killed.is_none() && end_by.is_none();
        tokio::select! {
            event = events.recv() => match event {
                Some(ChildEvent::Ready { admin: at }) => {
                    if admin.is_none() {
                        log.line(&format!("daemon ready (admin listener {at})"));
                        next_check = Instant::now() + policy.liveness_interval;
                    }
                    admin = Some(at);
                }
                Some(ChildEvent::Exited(info)) => return decide(&info, killed, !ending.is_empty(), log),
                None => {
                    let info = ExitInfo { code: None, stopped: None, stderr_tail: String::new(), exited_at_ms: 0 };
                    return decide(&info, killed, !ending.is_empty(), log);
                }
            },
            _ = tokio::time::sleep_until(ready_by), if admin.is_none() && quiet => {
                log.line(&format!("the daemon did not report ready within {:?}; killing its process tree", policy.ready_timeout));
                killed = Some("not ready");
                control.kill();
            }
            _ = tokio::time::sleep_until(next_check), if admin.is_some() && check.is_none() && quiet => {
                let at = admin.expect("guarded");
                let timeout = policy.liveness_timeout;
                let probe_check = probe.check(at);
                check = Some(Box::pin(async move {
                    match tokio::time::timeout(timeout, probe_check).await {
                        Ok(result) => result,
                        Err(_) => Err(format!("no answer within {timeout:?}")),
                    }
                }));
            }
            result = async { check.as_mut().expect("guarded").await }, if check.is_some() => {
                check = None;
                next_check = Instant::now() + policy.liveness_interval;
                if !quiet {
                    continue;
                }
                match result {
                    Ok(()) => {
                        if failures > 0 {
                            log.line(&format!("liveness restored after {failures} failed check(s)"));
                        }
                        failures = 0;
                    }
                    Err(e) => {
                        failures += 1;
                        log.line(&format!("liveness check failed ({failures}/{}): {e}", policy.liveness_failures));
                        if failures >= policy.liveness_failures {
                            log.line("the daemon is not live; killing its process tree");
                            killed = Some("not live");
                            control.kill();
                        }
                    }
                }
            }
            request = end_session.recv(), if end_open => match request {
                Some(request) => {
                    let deadline = Instant::now() + request.remaining();
                    if end_by.is_none() {
                        log.line("Windows is ending the session; passing it on to the daemon");
                        control.end_session();
                    }
                    end_by = Some(end_by.map_or(deadline, |d| d.min(deadline)));
                    ending.push(request);
                }
                None => end_open = false,
            },
            _ = tokio::time::sleep_until(end_by.unwrap_or(ready_by)), if end_by.is_some() && killed.is_none() => {
                log.line("the daemon did not stop before the end-session deadline; killing its process tree");
                killed = Some("end-session deadline");
                control.kill();
            }
        }
    }
}

fn decide(info: &ExitInfo, killed: Option<&str>, session_ending: bool, log: &dyn Log) -> Outcome {
    let tail = info.stderr_tail.trim();
    if session_ending {
        log.line(&format!(
            "daemon {} while the session ends; the watchdog exits",
            info.describe()
        ));
        return Outcome::Exit(Ended::SessionEnded);
    }
    if let Some(reason) = killed {
        log.line(&format!(
            "daemon killed ({reason}): {}. stderr: {tail}",
            info.describe()
        ));
        return Outcome::Restart;
    }
    match info.code {
        Some(EXIT_STOPPED) if info.stopped.is_none() => {
            log.line("daemon stopped on request; the watchdog exits");
            Outcome::Exit(Ended::Stopped)
        }
        Some(EXIT_CONFIG) => {
            log.line(&format!("daemon refused to start (configuration or startup error; retrying cannot fix it): {tail}"));
            Outcome::Exit(Ended::ConfigError)
        }
        _ => {
            log.line(&format!("daemon {}. stderr: {tail}", info.describe()));
            Outcome::Restart
        }
    }
}

// ----- the real daemon ---------------------------------------------------------------------

fn daemon_exe() -> anyhow::Result<PathBuf> {
    let name = if cfg!(windows) {
        "agent-app-server.exe"
    } else {
        "agent-app-server"
    };
    let exe = exe_dir()?.join(name);
    anyhow::ensure!(
        exe.is_file(),
        "{} not found next to the watchdog",
        exe.display()
    );
    Ok(exe)
}

/// The ready line the daemon writes on stdout.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReadyLine {
    event: String,
    admin_listen: SocketAddr,
}

/// Starts `agent-app-server run --background` under the watchdog's supervisor.
pub struct DaemonLauncher {
    supervisor: Supervisor,
    exe: PathBuf,
    paths: Paths,
}

struct DaemonControl {
    handle: ChildHandle,
    control: mpsc::UnboundedSender<&'static str>,
}

impl ChildControl for DaemonControl {
    fn pid(&self) -> u32 {
        self.handle.pid()
    }

    fn kill(&self) {
        self.handle.kill(StopReason::User);
    }

    fn end_session(&self) {
        if self.control.send(END_SESSION_LINE).is_err() {
            tracing::warn!("the daemon's stdin is closed; it relies on its own end-session window");
        }
    }
}

impl Launcher for DaemonLauncher {
    async fn launch(&mut self) -> anyhow::Result<Launched> {
        let spec = SpawnSpec::new("daemon", &self.exe, &self.paths.data_dir)
            .arg("--config-dir")
            .arg(&self.paths.config_dir)
            .arg("--data-dir")
            .arg(&self.paths.data_dir)
            .arg("run")
            .arg("--background");
        let mut child = self
            .supervisor
            .spawn(spec)
            .await
            .context("starting the daemon")?;
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        if let Some(stdout) = child.stdout.take() {
            // The daemon logs to files; stdout carries only its ready line. Reading it to the
            // end also keeps the daemon from ever blocking on a full pipe.
            let events = events_tx.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stdout).lines();
                loop {
                    match lines.next_line().await {
                        Ok(Some(line)) => match serde_json::from_str::<ReadyLine>(&line) {
                            Ok(ready) if ready.event == "ready" => {
                                let _ = events.send(ChildEvent::Ready {
                                    admin: ready.admin_listen,
                                });
                            }
                            _ => tracing::debug!(line, "unexpected output of the daemon"),
                        },
                        Ok(None) => return,
                        Err(e) => {
                            tracing::debug!(error = %e, "reading the daemon's stdout failed");
                            return;
                        }
                    }
                }
            });
        }
        let (control_tx, mut control_rx) = mpsc::unbounded_channel::<&'static str>();
        if let Some(mut stdin) = child.stdin.take() {
            tokio::spawn(async move {
                while let Some(line) = control_rx.recv().await {
                    let written = async {
                        stdin.write_all(line.as_bytes()).await?;
                        stdin.write_all(b"\n").await?;
                        stdin.flush().await
                    };
                    if let Err(e) = written.await {
                        tracing::warn!(error = %e, line, "could not write a control line to the daemon");
                        return;
                    }
                }
            });
        }
        let handle = child.handle.clone();
        tokio::spawn(async move {
            let info = handle.wait().await;
            let _ = events_tx.send(ChildEvent::Exited(info));
        });
        Ok(Launched {
            control: Box::new(DaemonControl {
                handle: child.handle,
                control: control_tx,
            }),
            events: events_rx,
        })
    }
}

/// Liveness checks through the admin client.
pub struct AdminProbe {
    pub connect_timeout: Duration,
    pub timeout: Duration,
}

impl Probe for AdminProbe {
    fn check(&self, admin: SocketAddr) -> CheckFuture {
        let client = AdminClient::new(
            admin,
            None,
            Timeouts {
                connect: self.connect_timeout.min(self.timeout),
                request: self.timeout,
            },
        );
        Box::pin(async move { client.liveness().await.map_err(|e| format!("{e:#}")) })
    }
}

/// `logs\watchdog.log`, through the watchdog's log subscriber (see [`init_logging`]), which
/// also records what the watchdog's supervisor reports.
pub struct FileLog;

impl Log for FileLog {
    fn line(&self, line: &str) {
        tracing::info!(target: "watchdog", "{line}");
    }
}

/// Sends the watchdog's logs (its own lines and the supervisor's events) to
/// `logs\watchdog.log`. Writes are unbuffered: the watchdog ends with `process::exit`.
pub fn init_logging(paths: &Paths) -> anyhow::Result<()> {
    use tracing_appender::rolling::{RollingFileAppender, Rotation};
    std::fs::create_dir_all(paths.logs_dir())
        .with_context(|| format!("creating {}", paths.logs_dir().display()))?;
    let appender = RollingFileAppender::builder()
        .rotation(Rotation::NEVER)
        .filename_prefix("watchdog.log")
        .build(paths.logs_dir())
        .context("opening watchdog.log")?;
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(appender)
        .with_ansi(false)
        .try_init()
        .map_err(|e| anyhow::anyhow!("installing the log subscriber: {e}"))
}

/// The configuration the watchdog takes its policy values from (`[policy]`: its own timings
/// and those of its supervisor). When config.toml does not exist yet (the daemon writes it on
/// its first start) or cannot be used, the documented defaults are used and that is logged;
/// a daemon that cannot use the file refuses to start with exit code 2, which ends the
/// watchdog too.
pub fn load_config(paths: &Paths, log: &dyn Log) -> Config {
    let file = paths.config_file();
    match file.try_exists() {
        Ok(true) => {}
        Ok(false) => {
            log.line(&format!(
                "{} does not exist yet (the daemon writes it on its first start); the watchdog uses the default policy values",
                file.display()
            ));
            return Config::default();
        }
        Err(e) => {
            log.line(&format!(
                "{} cannot be checked ({e}); the watchdog uses the default policy values",
                file.display()
            ));
            return Config::default();
        }
    }
    match Config::load(&file) {
        Ok(config) => config,
        Err(e) => {
            log.line(&format!(
                "config.toml cannot be used ({e:#}); the watchdog uses the default policy values"
            ));
            Config::default()
        }
    }
}

/// The record of a watchdog that ended on purpose (see the module documentation).
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct StoppedRecord {
    /// `stopped` or `configError`.
    reason: String,
    /// When (milliseconds since the Unix epoch).
    at: i64,
}

/// Writes the record of a deliberate end. A failure is logged: the keep-alive task then starts
/// the watchdog again, which is what it does for a crash.
fn record_deliberate_end(paths: &Paths, ended: Ended, log: &dyn Log) {
    let record = StoppedRecord {
        reason: match ended {
            Ended::ConfigError => "configError",
            Ended::Stopped | Ended::SessionEnded => "stopped",
        }
        .into(),
        at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0),
    };
    let file = paths.watchdog_stopped_file();
    let written = serde_json::to_vec(&record)
        .map_err(std::io::Error::other)
        .and_then(|bytes| std::fs::write(&file, bytes));
    if let Err(e) = written {
        log.line(&format!(
            "could not write {} ({e}); the keep-alive task may start the watchdog again",
            file.display()
        ));
    }
}

/// What a start does about the record of a deliberate end.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartDecision {
    /// Supervise the daemon.
    Run,
    /// A keep-alive start after a deliberate end: exit without starting the daemon.
    StayStopped(String),
}

/// An explicit start removes the record; a keep-alive start honours it. A record that cannot
/// be checked keeps a keep-alive start from starting (the next one tries again).
pub fn start_decision(paths: &Paths, keepalive: bool, log: &dyn Log) -> StartDecision {
    let file = paths.watchdog_stopped_file();
    if !keepalive {
        match std::fs::remove_file(&file) {
            Ok(()) => log.line("explicit start: the record of the last deliberate stop is removed"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => log.line(&format!("could not remove {} ({e})", file.display())),
        }
        return StartDecision::Run;
    }
    match std::fs::read(&file) {
        Ok(bytes) => {
            let what = serde_json::from_slice::<StoppedRecord>(&bytes)
                .map(|r| format!("it ended on purpose ({})", r.reason))
                .unwrap_or_else(|_| "it ended on purpose".into());
            StartDecision::StayStopped(what)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => StartDecision::Run,
        Err(e) => {
            let why = format!("{} cannot be read ({e})", file.display());
            log.line(&format!(
                "keep-alive start: {why}; not starting (the next keep-alive start tries again)"
            ));
            StartDecision::StayStopped(why)
        }
    }
}

/// How a start of the watchdog begins (see [`begin`]).
pub enum Begin {
    /// Supervise the daemon, holding `watchdog.lock` for as long as the watchdog runs.
    Run(InstanceLock),
    /// Another watchdog holds `watchdog.lock`.
    AlreadyRunning,
    /// A keep-alive start after a deliberate end: exit without starting the daemon.
    StayStopped(String),
    /// `watchdog.lock` cannot be used.
    Failed(anyhow::Error),
}

/// Settles the record of a deliberate end and takes `watchdog.lock` (one watchdog per data
/// folder: a second one would sweep the first one's ledger, whose entries are alive,
/// terminating the running daemon and every agent with it).
///
/// The record is settled before the lock is tried. An explicit start removes it and a
/// keep-alive start that honours it exits without ever holding the lock. The keep-alive task
/// fires next to an explicit start (right after `autostart install` registers it, and at
/// logon when it catches up on runs missed while the user was signed out); a keep-alive start
/// that held the lock while it read the record would make the explicit start exit as "already
/// running" before removing the record, leaving no watchdog and a record that every later
/// keep-alive start honours. Removed first, an explicit start that finds the lock held leaves
/// no record behind, so the next keep-alive start runs the watchdog if nothing else does.
///
/// A keep-alive start checks the record once more after it took the lock: a watchdog that
/// ended on purpose between the two checks wrote the record while it still held the lock.
pub fn begin(paths: &Paths, keepalive: bool, log: &dyn Log) -> Begin {
    if let StartDecision::StayStopped(why) = start_decision(paths, keepalive, log) {
        return Begin::StayStopped(why);
    }
    let lock = match try_lock_file(&paths.watchdog_lock_file()) {
        Ok(Some(lock)) => lock,
        Ok(None) => return Begin::AlreadyRunning,
        Err(e) => return Begin::Failed(e),
    };
    if keepalive && let StartDecision::StayStopped(why) = start_decision(paths, true, log) {
        return Begin::StayStopped(why);
    }
    Begin::Run(lock)
}

/// Runs the watchdog for `paths`; `keepalive` for a start by the keep-alive task. Returns its
/// exit code.
pub async fn run(paths: Paths, keepalive: bool) -> i32 {
    if init_logging(&paths).is_err() {
        // Without a console and without its log file the watchdog could not report anything;
        // the data folder is unusable, which a retry does not fix.
        return EXIT_CONFIG;
    }
    let log = FileLog;
    let _lock = match begin(&paths, keepalive, &log) {
        Begin::Run(lock) => lock,
        Begin::AlreadyRunning => {
            if !keepalive {
                log.line("another watchdog is already running for this data folder; exiting");
            }
            return EXIT_ALREADY_RUNNING;
        }
        Begin::StayStopped(why) => {
            // Every few minutes until the next explicit start: not worth a line in
            // watchdog.log each time (the line of the deliberate end is there already).
            tracing::debug!(target: "watchdog", why, "keep-alive start: the watchdog stays stopped");
            return EXIT_STOPPED;
        }
        Begin::Failed(e) => {
            log.line(&format!("{e:#}"));
            return EXIT_FAILURE;
        }
    };
    log.line(if keepalive {
        "watchdog started by the keep-alive task (the previous one ended without being stopped)"
    } else {
        "watchdog started"
    });
    let exe = match daemon_exe() {
        Ok(exe) => exe,
        Err(e) => {
            log.line(&format!("{e:#}; the watchdog exits"));
            record_deliberate_end(&paths, Ended::ConfigError, &log);
            return EXIT_CONFIG;
        }
    };
    let config = load_config(&paths, &log);
    let policy = config.daemon_policy.clone();
    let supervisor = match Supervisor::new(
        &paths.data_dir.join("watchdog"),
        aas_supervisor::SupervisorPolicy {
            prevent_sleep: false,
            ..config.policy.supervisor_policy()
        },
    ) {
        Ok(s) => s,
        Err(e) => {
            log.line(&format!("starting the watchdog supervisor failed: {e}"));
            return EXIT_FAILURE;
        }
    };
    let sweep = supervisor.sweep_orphans();
    if !sweep.terminated.is_empty() {
        log.line(&format!(
            "terminated {} process(es) left over from a previous watchdog",
            sweep.terminated.len()
        ));
    }
    let (end_tx, end_rx) = mpsc::unbounded_channel();
    #[cfg(windows)]
    let _window =
        match crate::endsession::EndSessionWindow::start(policy.end_session_deadline, end_tx) {
            Ok(window) => Some(window),
            Err(e) => {
                log.line(&format!("cannot listen for the end of the session: {e}"));
                None
            }
        };
    #[cfg(not(windows))]
    drop(end_tx);
    let probe = AdminProbe {
        connect_timeout: policy.admin_connect_timeout,
        timeout: policy.liveness_timeout,
    };
    let mut launcher = DaemonLauncher {
        supervisor,
        exe,
        paths: paths.clone(),
    };
    let ended = supervise(
        &mut launcher,
        &probe,
        &WatchdogPolicy::from(&policy),
        &log,
        end_rx,
    )
    .await;
    if ended.is_deliberate() {
        record_deliberate_end(&paths, ended, &log);
    }
    ended.exit_code()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use parking_lot::Mutex;

    #[test]
    fn a_keepalive_start_honours_a_deliberate_end_until_an_explicit_start() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_dir: dir.path().join("cfg"),
            data_dir: dir.path().to_path_buf(),
        };
        let log = MemLog::default();
        assert_eq!(
            start_decision(&paths, true, &log),
            StartDecision::Run,
            "nothing recorded: the watchdog crashed, start it"
        );
        record_deliberate_end(&paths, Ended::Stopped, &log);
        assert!(matches!(
            start_decision(&paths, true, &log),
            StartDecision::StayStopped(why) if why.contains("stopped")
        ));
        record_deliberate_end(&paths, Ended::ConfigError, &log);
        assert!(matches!(
            start_decision(&paths, true, &log),
            StartDecision::StayStopped(why) if why.contains("configError")
        ));
        // The logon task, `schtasks /Run`, `autostart install`: an explicit start.
        assert_eq!(start_decision(&paths, false, &log), StartDecision::Run);
        assert!(!paths.watchdog_stopped_file().exists());
        assert_eq!(start_decision(&paths, true, &log), StartDecision::Run);
        assert!(!Ended::SessionEnded.is_deliberate());
        assert_eq!(Ended::SessionEnded.exit_code(), EXIT_STOPPED);
    }

    #[test]
    fn a_keepalive_start_that_stays_stopped_never_blocks_an_explicit_start() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_dir: dir.path().join("cfg"),
            data_dir: dir.path().to_path_buf(),
        };
        let log = MemLog::default();
        record_deliberate_end(&paths, Ended::Stopped, &log);
        // Someone holds the lock: a keep-alive start that fired at the same moment, or a
        // watchdog that is still ending.
        let held = try_lock_file(&paths.watchdog_lock_file()).unwrap().unwrap();
        // A keep-alive start honours the record without trying the lock.
        assert!(matches!(
            begin(&paths, true, &log),
            Begin::StayStopped(why) if why.contains("stopped")
        ));
        // An explicit start that finds the lock held has removed the record all the same.
        assert!(matches!(begin(&paths, false, &log), Begin::AlreadyRunning));
        assert!(!paths.watchdog_stopped_file().exists());
        drop(held);
        // So the next keep-alive start runs the watchdog, and holds the lock while it runs.
        let Begin::Run(lock) = begin(&paths, true, &log) else {
            panic!("the keep-alive start runs");
        };
        assert!(matches!(begin(&paths, true, &log), Begin::AlreadyRunning));
        assert!(matches!(begin(&paths, false, &log), Begin::AlreadyRunning));
        drop(lock);
        // Without a record and without a watchdog, an explicit start runs.
        assert!(matches!(begin(&paths, false, &log), Begin::Run(_)));
    }

    #[test]
    fn the_restart_delay_doubles_up_to_the_cap_and_resets_after_a_stable_run() {
        let mut b = Backoff::new(
            Duration::from_secs(2),
            Duration::from_secs(60),
            Duration::from_secs(600),
        );
        let short = Duration::from_secs(1);
        let delays: Vec<u64> = (0..8).map(|_| b.after_run(short).as_secs()).collect();
        assert_eq!(delays, vec![2, 4, 8, 16, 32, 60, 60, 60]);
        assert_eq!(
            b.after_run(Duration::from_secs(600)).as_secs(),
            2,
            "a stable run resets the delay"
        );
        assert_eq!(b.after_run(short).as_secs(), 4);
    }

    /// What a stand-in daemon does once started.
    #[derive(Debug, Clone)]
    enum Script {
        /// Reports ready, then exits with `code` after `after`.
        ReadyThenExit { after: Duration, code: i32 },
        /// Exits with `code` without reporting ready.
        Exit { code: i32 },
        /// Reports ready and runs until killed or told that the session ends; then exits with
        /// `end_code` after `end_delay`.
        Serve { end_delay: Duration, end_code: i32 },
        /// Never reports ready.
        Hang,
    }

    #[derive(Default)]
    struct Record {
        starts: Vec<Instant>,
        kills: Vec<Instant>,
        end_sessions: usize,
    }

    struct FakeControl {
        record: Arc<Mutex<Record>>,
        events: mpsc::UnboundedSender<ChildEvent>,
        script: Script,
    }

    fn exit(code: Option<i32>, stopped: Option<StopReason>) -> ChildEvent {
        ChildEvent::Exited(ExitInfo {
            code,
            stopped,
            stderr_tail: String::new(),
            exited_at_ms: 0,
        })
    }

    impl ChildControl for FakeControl {
        fn pid(&self) -> u32 {
            4242
        }

        fn kill(&self) {
            self.record.lock().kills.push(Instant::now());
            let _ = self.events.send(exit(Some(1), Some(StopReason::User)));
        }

        fn end_session(&self) {
            self.record.lock().end_sessions += 1;
            if let Script::Serve {
                end_delay,
                end_code,
            } = self.script
            {
                let events = self.events.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(end_delay).await;
                    let _ = events.send(exit(Some(end_code), None));
                });
            }
        }
    }

    struct FakeLauncher {
        scripts: VecDeque<Script>,
        record: Arc<Mutex<Record>>,
    }

    fn admin() -> SocketAddr {
        "127.0.0.1:7879".parse().unwrap()
    }

    impl Launcher for FakeLauncher {
        async fn launch(&mut self) -> anyhow::Result<Launched> {
            let script = self
                .scripts
                .pop_front()
                .expect("no more stand-in daemons scripted");
            self.record.lock().starts.push(Instant::now());
            let (tx, rx) = mpsc::unbounded_channel();
            match script.clone() {
                Script::ReadyThenExit { after, code } => {
                    let _ = tx.send(ChildEvent::Ready { admin: admin() });
                    let events = tx.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(after).await;
                        let _ = events.send(exit(Some(code), None));
                    });
                }
                Script::Exit { code } => {
                    let _ = tx.send(exit(Some(code), None));
                }
                Script::Serve { .. } => {
                    let _ = tx.send(ChildEvent::Ready { admin: admin() });
                }
                Script::Hang => {}
            }
            Ok(Launched {
                control: Box::new(FakeControl {
                    record: self.record.clone(),
                    events: tx,
                    script,
                }),
                events: rx,
            })
        }
    }

    /// Answers checks from a script (`true` = live); live once the script is used up.
    struct FakeProbe {
        answers: Mutex<VecDeque<bool>>,
        checks: AtomicUsize,
    }

    impl FakeProbe {
        fn new(answers: &[bool]) -> Self {
            Self {
                answers: Mutex::new(answers.iter().copied().collect()),
                checks: AtomicUsize::new(0),
            }
        }
    }

    impl Probe for FakeProbe {
        fn check(&self, _admin: SocketAddr) -> CheckFuture {
            self.checks.fetch_add(1, Ordering::SeqCst);
            let live = self.answers.lock().pop_front().unwrap_or(true);
            Box::pin(async move {
                if live {
                    Ok(())
                } else {
                    Err("503 unavailable".into())
                }
            })
        }
    }

    #[derive(Default)]
    struct MemLog(Mutex<Vec<String>>);

    impl Log for MemLog {
        fn line(&self, line: &str) {
            self.0.lock().push(line.to_owned());
        }
    }

    fn policy() -> WatchdogPolicy {
        WatchdogPolicy {
            restart_delay_min: Duration::from_secs(2),
            restart_delay_max: Duration::from_secs(60),
            stable_run: Duration::from_secs(600),
            ready_timeout: Duration::from_secs(120),
            liveness_interval: Duration::from_secs(30),
            liveness_timeout: Duration::from_secs(10),
            liveness_failures: 3,
        }
    }

    async fn run_scripts(
        scripts: Vec<Script>,
        probe: &FakeProbe,
    ) -> (
        Ended,
        Arc<Mutex<Record>>,
        Vec<String>,
        mpsc::UnboundedSender<EndSession>,
    ) {
        let record = Arc::new(Mutex::new(Record::default()));
        let mut launcher = FakeLauncher {
            scripts: scripts.into(),
            record: record.clone(),
        };
        let log = MemLog::default();
        let (end_tx, end_rx) = mpsc::unbounded_channel();
        let code = supervise(&mut launcher, probe, &policy(), &log, end_rx).await;
        let lines = log.0.lock().clone();
        (code, record, lines, end_tx)
    }

    fn gaps(times: &[Instant]) -> Vec<u64> {
        times.windows(2).map(|w| (w[1] - w[0]).as_secs()).collect()
    }

    #[tokio::test(start_paused = true)]
    async fn crashes_are_restarted_with_the_backoff_and_a_requested_stop_ends_the_watchdog() {
        let probe = FakeProbe::new(&[]);
        let crash = |code| Script::Exit { code };
        let scripts = vec![
            crash(1),
            crash(101),
            crash(crate::EXIT_BUSY),
            crash(1),
            Script::ReadyThenExit {
                after: Duration::from_secs(5),
                code: 0,
            },
        ];
        let (code, record, lines, _) = run_scripts(scripts, &probe).await;
        assert_eq!(code, Ended::Stopped);
        assert!(code.is_deliberate());
        let record = record.lock();
        assert_eq!(gaps(&record.starts), vec![2, 4, 8, 16], "{lines:#?}");
        assert!(lines.iter().any(|l| l.contains("stopped on request")));
    }

    #[tokio::test(start_paused = true)]
    async fn a_configuration_error_ends_the_watchdog_without_a_retry() {
        let probe = FakeProbe::new(&[]);
        let (code, record, lines, _) = run_scripts(
            vec![Script::Exit { code: 1 }, Script::Exit { code: EXIT_CONFIG }],
            &probe,
        )
        .await;
        assert_eq!(code, Ended::ConfigError);
        assert_eq!(code.exit_code(), EXIT_CONFIG);
        assert!(code.is_deliberate());
        assert_eq!(record.lock().starts.len(), 2);
        assert!(
            lines.last().unwrap().contains("configuration"),
            "{lines:#?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_stable_run_resets_the_delay() {
        let probe = FakeProbe::new(&[]);
        let scripts = vec![
            Script::Exit { code: 1 },
            Script::Exit { code: 1 },
            Script::ReadyThenExit {
                after: Duration::from_secs(700),
                code: 1,
            },
            Script::Exit { code: 1 },
            Script::Exit { code: 0 },
        ];
        let (code, record, _, _) = run_scripts(scripts, &probe).await;
        assert_eq!(code, Ended::Stopped);
        // 2 s, 4 s, then the 700 s run plus a reset delay of 2 s, then 4 s.
        assert_eq!(gaps(&record.lock().starts), vec![2, 4, 702, 4]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_daemon_that_stops_being_live_is_killed_and_restarted() {
        // Live once, then three failed checks in a row.
        let probe = FakeProbe::new(&[true, false, false, false]);
        let scripts = vec![
            Script::Serve {
                end_delay: Duration::ZERO,
                end_code: 0,
            },
            Script::Exit { code: 0 },
        ];
        let (code, record, lines, _) = run_scripts(scripts, &probe).await;
        assert_eq!(code, Ended::Stopped);
        let record = record.lock();
        assert_eq!(record.kills.len(), 1, "{lines:#?}");
        // Checks at 30, 60, 90 and 120 s; the third consecutive failure (at 120 s) kills it.
        assert_eq!((record.kills[0] - record.starts[0]).as_secs(), 120);
        assert_eq!(probe.checks.load(Ordering::SeqCst), 4);
        assert_eq!(
            (record.starts[1] - record.kills[0]).as_secs(),
            2,
            "restarted with the normal backoff"
        );
        assert!(lines.iter().any(|l| l.contains("not live")));
    }

    #[tokio::test(start_paused = true)]
    async fn a_failure_streak_is_reset_by_a_successful_check() {
        let probe = FakeProbe::new(&[false, false, true, false, false, true]);
        let scripts = vec![Script::Serve {
            end_delay: Duration::ZERO,
            end_code: 0,
        }];
        let record = Arc::new(Mutex::new(Record::default()));
        let mut launcher = FakeLauncher {
            scripts: scripts.into(),
            record: record.clone(),
        };
        let log = MemLog::default();
        let (end_tx, end_rx) = mpsc::unbounded_channel();
        let p = policy();
        let supervising = supervise(&mut launcher, &probe, &p, &log, end_rx);
        let stop = async {
            // Six checks happen within 200 s; then the session ends.
            tokio::time::sleep(Duration::from_secs(200)).await;
            end_tx
                .send(EndSession::unobserved(
                    std::time::Instant::now() + Duration::from_secs(4),
                ))
                .unwrap();
        };
        let (code, ()) = tokio::join!(supervising, stop);
        assert_eq!(code, Ended::SessionEnded);
        assert!(
            record.lock().kills.is_empty(),
            "never three failures in a row"
        );
        assert!(probe.checks.load(Ordering::SeqCst) >= 6);
    }

    #[tokio::test(start_paused = true)]
    async fn a_daemon_that_never_reports_ready_is_killed_after_the_ready_timeout() {
        let probe = FakeProbe::new(&[]);
        let (code, record, _, _) =
            run_scripts(vec![Script::Hang, Script::Exit { code: 0 }], &probe).await;
        assert_eq!(code, Ended::Stopped);
        let record = record.lock();
        assert_eq!((record.kills[0] - record.starts[0]).as_secs(), 120);
        assert_eq!(
            probe.checks.load(Ordering::SeqCst),
            0,
            "no liveness checks before the ready line"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_end_of_the_session_is_passed_on_and_nothing_is_restarted() {
        let probe = FakeProbe::new(&[]);
        let record = Arc::new(Mutex::new(Record::default()));
        // The daemon exits with a failure code once told: still no restart.
        let mut launcher = FakeLauncher {
            scripts: vec![Script::Serve {
                end_delay: Duration::from_secs(1),
                end_code: 1,
            }]
            .into(),
            record: record.clone(),
        };
        let log = MemLog::default();
        let (end_tx, end_rx) = mpsc::unbounded_channel();
        let p = policy();
        let (request, waiter) =
            EndSession::with_waiter(std::time::Instant::now() + Duration::from_secs(4));
        let supervising = supervise(&mut launcher, &probe, &p, &log, end_rx);
        let send = async {
            tokio::time::sleep(Duration::from_secs(10)).await;
            end_tx.send(request).unwrap();
        };
        let (code, ()) = tokio::join!(supervising, send);
        assert_eq!(code, Ended::SessionEnded);
        assert!(!code.is_deliberate(), "the next logon starts it anyway");
        let record = record.lock();
        assert_eq!(record.end_sessions, 1);
        assert_eq!(record.starts.len(), 1);
        assert!(record.kills.is_empty());
        assert!(
            waiter.wait_until(std::time::Instant::now()),
            "acknowledged when the watchdog exits"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_daemon_that_misses_the_end_session_deadline_is_killed() {
        let probe = FakeProbe::new(&[]);
        let record = Arc::new(Mutex::new(Record::default()));
        let mut launcher = FakeLauncher {
            scripts: vec![Script::Serve {
                end_delay: Duration::from_secs(60),
                end_code: 0,
            }]
            .into(),
            record: record.clone(),
        };
        let log = MemLog::default();
        let (end_tx, end_rx) = mpsc::unbounded_channel();
        end_tx
            .send(EndSession::unobserved(
                std::time::Instant::now() + Duration::from_secs(4),
            ))
            .unwrap();
        let code = supervise(&mut launcher, &probe, &policy(), &log, end_rx).await;
        assert_eq!(code, Ended::SessionEnded);
        let record = record.lock();
        assert_eq!(record.kills.len(), 1);
        assert!((record.kills[0] - record.starts[0]) <= Duration::from_secs(5));
    }

    #[tokio::test(start_paused = true)]
    async fn the_end_of_the_session_during_the_restart_delay_ends_the_watchdog() {
        let probe = FakeProbe::new(&[]);
        let record = Arc::new(Mutex::new(Record::default()));
        let mut launcher = FakeLauncher {
            scripts: vec![Script::Exit { code: 1 }].into(),
            record: record.clone(),
        };
        let log = MemLog::default();
        let (end_tx, end_rx) = mpsc::unbounded_channel();
        let p = policy();
        let supervising = supervise(&mut launcher, &probe, &p, &log, end_rx);
        let send = async {
            tokio::time::sleep(Duration::from_secs(1)).await;
            end_tx
                .send(EndSession::unobserved(
                    std::time::Instant::now() + Duration::from_secs(4),
                ))
                .unwrap();
        };
        let (code, ()) = tokio::join!(supervising, send);
        assert_eq!(code, Ended::SessionEnded);
        assert!(!code.is_deliberate(), "the next logon starts it anyway");
        assert_eq!(record.lock().starts.len(), 1, "not restarted");
    }
}
