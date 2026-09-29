use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;
use process_wrap::tokio::*;
use tokio::io::AsyncReadExt;
use tokio::process::{ChildStdin, ChildStdout, Command};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use crate::ledger::{Ledger, LedgerEntry};
use crate::tail::TailBuffer;
use crate::{SpawnError, SpawnSpec, SupervisorPolicy, platform};

/// Size of one read from a child's stderr pipe. A pure buffer size: what is kept of stderr is
/// bounded by the policy value `stderr_tail_bytes`, and reads continue until the pipe ends.
const STDERR_READ_CHUNK_BYTES: usize = 8 * 1024;

/// Why the supervisor terminated a process tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StopReason {
    /// The user stopped the thread.
    User,
    /// Idle process reaping.
    Idle,
    /// Daemon shutdown.
    Shutdown,
    /// The harness did not honour an interrupt within the grace period.
    InterruptTimeout,
    /// Every handle to the child was dropped; nobody can talk to it any more.
    Abandoned,
}

impl StopReason {
    pub fn as_str(self) -> &'static str {
        match self {
            StopReason::User => "user",
            StopReason::Idle => "idle",
            StopReason::Shutdown => "shutdown",
            StopReason::InterruptTimeout => "interruptTimeout",
            StopReason::Abandoned => "abandoned",
        }
    }
}

/// How a supervised process tree ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExitInfo {
    /// Exit code of the main process (`None` when it could not be determined).
    pub code: Option<i32>,
    /// Set when the supervisor terminated the tree before the main process exited on its own.
    pub stopped: Option<StopReason>,
    /// Last bytes the process wrote to stderr.
    pub stderr_tail: String,
    pub exited_at_ms: i64,
}

impl ExitInfo {
    /// The main process exited by itself with code 0.
    pub fn is_clean(&self) -> bool {
        self.stopped.is_none() && self.code == Some(0)
    }

    /// Human-readable summary (`exited with code 1`, `stopped (idle)`, …).
    pub fn describe(&self) -> String {
        match (self.stopped, self.code) {
            (Some(reason), _) => format!("stopped ({})", reason.as_str()),
            (None, Some(code)) => format!("exited with code {code}"),
            (None, None) => "exited (code unknown)".to_owned(),
        }
    }
}

pub(crate) fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

enum ChildCommand {
    Kill(StopReason),
}

/// Cloneable control handle of a supervised child. When every clone is dropped the tree is
/// terminated ([`StopReason::Abandoned`]), so a forgotten handle cannot leak processes.
#[derive(Clone)]
pub struct ChildHandle {
    pid: u32,
    label: Arc<str>,
    cmd_tx: mpsc::UnboundedSender<ChildCommand>,
    exit_rx: watch::Receiver<Option<ExitInfo>>,
    stderr_tail: Arc<Mutex<TailBuffer>>,
    kill_confirm_timeout: Duration,
    /// The job holding every process of the tree.
    #[cfg(windows)]
    tree: Option<Arc<platform::TreeJob>>,
}

impl std::fmt::Debug for ChildHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChildHandle")
            .field("pid", &self.pid)
            .field("label", &self.label)
            .finish()
    }
}

impl ChildHandle {
    pub fn pid(&self) -> u32 {
        self.pid
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    /// Exit information if the tree has already ended.
    pub fn exit_info(&self) -> Option<ExitInfo> {
        self.exit_rx.borrow().clone()
    }

    /// Current stderr tail (for diagnostics while the process runs).
    pub fn stderr_tail(&self) -> String {
        self.stderr_tail.lock().to_string_lossy()
    }

    /// Whether the process `pid` created at `created` (see
    /// [`process_creation_time`](crate::process_creation_time)) belongs to this child's tree:
    /// the job every process the child starts belongs to, whatever jobs of its own a program
    /// creates, as long as it cannot break away (the supervisor does not allow that).
    /// `Ok(false)` for a process that does not exist (any more). A diagnostic, e.g. to check that
    /// a CLI's sandboxed commands stay in the tree.
    #[cfg(windows)]
    pub fn tree_contains(&self, pid: u32, created: u64) -> std::io::Result<bool> {
        let tree = self
            .tree
            .as_ref()
            .ok_or_else(|| std::io::Error::other("the tree has no accounting job"))?;
        tree.contains_process(pid, created)
    }

    /// Resolves when the main process has exited and every remaining process of its job has
    /// been terminated and has exited (bounded by `kill_confirm_timeout`; a tree that outlives
    /// it is reported in the log).
    pub async fn wait(&self) -> ExitInfo {
        let mut rx = self.exit_rx.clone();
        loop {
            if let Some(info) = rx.borrow_and_update().clone() {
                return info;
            }
            if rx.changed().await.is_err() {
                // The supervising task is gone without publishing (it panicked).
                return rx.borrow().clone().unwrap_or_else(|| {
                    tracing::error!(pid = self.pid, label = %self.label, "the supervising task ended without reporting the exit");
                    ExitInfo {
                        code: None,
                        stopped: None,
                        stderr_tail: self.stderr_tail(),
                        exited_at_ms: now_ms(),
                    }
                });
            }
        }
    }

    /// Terminates the whole tree now.
    pub fn kill(&self, reason: StopReason) {
        // The supervising task receives commands until the tree has exited; a send that fails
        // finds the tree already gone, which is what the kill asks for.
        let _ = self.cmd_tx.send(ChildCommand::Kill(reason));
    }

    /// Staged stop: waits up to `grace` for the process to exit by itself (callers close the
    /// child's stdin and/or send a protocol-level cancel first), then terminates the tree.
    pub async fn shutdown(&self, grace: Duration, reason: StopReason) -> ExitInfo {
        if let Ok(info) = tokio::time::timeout(grace, self.wait()).await {
            return info;
        }
        tracing::info!(pid = self.pid, label = %self.label, reason = reason.as_str(), "grace period over; terminating process tree");
        self.kill(reason);
        match tokio::time::timeout(self.kill_confirm_timeout, self.wait()).await {
            Ok(info) => info,
            Err(_) => {
                tracing::error!(pid = self.pid, label = %self.label, "process tree did not exit after TerminateJobObject");
                ExitInfo {
                    code: None,
                    stopped: Some(reason),
                    stderr_tail: self.stderr_tail(),
                    exited_at_ms: now_ms(),
                }
            }
        }
    }
}

/// A freshly spawned child: its control handle plus the pipes the adapter speaks over.
pub struct ManagedChild {
    pub handle: ChildHandle,
    pub stdin: Option<ChildStdin>,
    pub stdout: Option<ChildStdout>,
}

pub(crate) async fn spawn(
    spec: SpawnSpec,
    policy: SupervisorPolicy,
    ledger: Ledger,
    running: Arc<AtomicUsize>,
) -> Result<ManagedChild, SpawnError> {
    if !spec.cwd.is_dir() {
        return Err(SpawnError::MissingCwd(spec.cwd));
    }
    let mut cmd = Command::new(&spec.program);
    cmd.args(&spec.args)
        .current_dir(&spec.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for key in &spec.env_remove {
        cmd.env_remove(key);
    }
    for (key, value) in &spec.env {
        cmd.env(key, value);
    }

    let mut wrap = CommandWrap::from(cmd);
    #[cfg(windows)]
    let tree_slot = TreeSlot::default();
    #[cfg(windows)]
    {
        wrap.wrap(JobObject)
            .wrap(KillOnDrop)
            .wrap(CreationFlags(
                windows::Win32::System::Threading::CREATE_NO_WINDOW,
            ))
            .wrap(TreeAccounting(tree_slot.clone()));
    }
    #[cfg(unix)]
    {
        wrap.wrap(ProcessGroup::leader()).wrap(KillOnDrop);
    }

    let mut child = wrap.spawn().map_err(|source| SpawnError::Io {
        program: spec.program.display().to_string(),
        source,
    })?;
    #[cfg(windows)]
    let tree = tree_slot.lock().take().map(Arc::new);

    // The ledger is what finds the tree after a daemon crash (design.md §4.5): a process that
    // cannot be recorded does not run.
    let tracked = identify(child.as_ref()).and_then(|(pid, created)| {
        ledger.record(LedgerEntry {
            pid,
            created,
            label: spec.label.clone(),
            owner: spec.owner.clone(),
            spawned_at_ms: now_ms(),
        })?;
        Ok(pid)
    });
    let pid = match tracked {
        Ok(pid) => pid,
        Err(source) => {
            tracing::error!(label = %spec.label, program = %spec.program.display(), error = %source, "cannot record the new process in the child ledger; terminating it");
            abandon_spawn(
                child,
                #[cfg(windows)]
                tree,
                policy.kill_confirm_timeout,
                &spec.label,
            )
            .await;
            return Err(SpawnError::Untracked {
                program: spec.program.display().to_string(),
                source,
            });
        }
    };
    let stdin = child.stdin().take();
    let stdout = child.stdout().take();
    let stderr = child.stderr().take();
    running.fetch_add(1, Ordering::SeqCst);
    tracing::info!(pid, label = %spec.label, program = %spec.program.display(), "spawned supervised process");

    let label: Arc<str> = Arc::from(spec.label.as_str());
    let tail = Arc::new(Mutex::new(TailBuffer::new(policy.stderr_tail_bytes)));
    let stderr_task = stderr.map(|mut pipe| {
        let tail = tail.clone();
        let label = label.clone();
        tokio::spawn(async move {
            let mut buf = vec![0u8; STDERR_READ_CHUNK_BYTES];
            loop {
                match pipe.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => {
                        tail.lock().push(&buf[..n]);
                        tracing::trace!(label = %label, bytes = n, "stderr");
                    }
                    Err(e) => {
                        // The pipe is closed when this task ends, so the child's further writes
                        // fail instead of blocking on a pipe nobody reads. The exit report says
                        // why its stderr stops here.
                        tracing::warn!(label = %label, error = %e, "reading the child's stderr failed; the rest of it is lost");
                        tail.lock().push(stderr_read_failure(&e).as_bytes());
                        break;
                    }
                }
            }
        })
    });

    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (exit_tx, exit_rx) = watch::channel(None);
    let handle = ChildHandle {
        pid,
        label: label.clone(),
        cmd_tx,
        exit_rx,
        stderr_tail: tail.clone(),
        kill_confirm_timeout: policy.kill_confirm_timeout,
        #[cfg(windows)]
        tree: tree.clone(),
    };
    tokio::spawn(supervise(
        child,
        cmd_rx,
        exit_tx,
        Supervised {
            pid,
            label,
            tail,
            stderr_task,
            ledger,
            running,
            kill_confirm_timeout: policy.kill_confirm_timeout,
            #[cfg(windows)]
            tree,
        },
    ));
    Ok(ManagedChild {
        handle,
        stdin,
        stdout,
    })
}

/// The note added to a child's stderr tail when its stderr could not be read to the end.
fn stderr_read_failure(error: &std::io::Error) -> String {
    format!("\n[aas-supervisor: reading stderr failed: {error}]\n")
}

/// PID and creation time of a freshly spawned process, read through the process handle the
/// spawn returned (so they are exact even if the process has already exited).
#[cfg(windows)]
fn identify(child: &dyn ChildWrapper) -> std::io::Result<(u32, u64)> {
    use std::os::windows::io::AsRawHandle;
    let pid = child
        .id()
        .ok_or_else(|| std::io::Error::other("the new process has no id"))?;
    let handle = child
        .process_handle()
        .ok_or_else(|| std::io::Error::other("the new process has no handle"))?;
    let created =
        platform::handle_creation_time(windows::Win32::Foundation::HANDLE(handle.as_raw_handle()))?;
    Ok((pid, created))
}

/// PID and start time of a freshly spawned process.
#[cfg(unix)]
fn identify(child: &dyn ChildWrapper) -> std::io::Result<(u32, u64)> {
    let pid = child
        .id()
        .ok_or_else(|| std::io::Error::other("the new process has no id"))?;
    let created = platform::process_creation_time(pid)
        .ok_or_else(|| std::io::Error::other("the start time of the new process is unreadable"))?;
    Ok((pid, created))
}

/// Terminates the tree of a spawn that cannot go on and waits (bounded) until it is gone, so
/// the error reaches the caller with no process of it left.
async fn abandon_spawn(
    mut child: Box<dyn ChildWrapper>,
    #[cfg(windows)] tree: Option<Arc<platform::TreeJob>>,
    timeout: Duration,
    label: &str,
) {
    if let Err(e) = child.start_kill() {
        tracing::warn!(label, error = %e, "terminating the abandoned process tree failed");
    }
    #[cfg(windows)]
    if let Some(tree) = tree {
        match tokio::task::spawn_blocking(move || tree.terminate_and_wait(timeout)).await {
            Ok(Ok(true)) => {}
            Ok(Ok(false)) => tracing::error!(
                label,
                "processes of the abandoned tree are still alive after termination"
            ),
            Ok(Err(e)) => tracing::warn!(
                label,
                error = %e,
                "cannot tell whether the abandoned tree has exited"
            ),
            Err(e) => tracing::error!(label, error = %e, "waiting for the abandoned tree failed"),
        }
    }
    if tokio::time::timeout(timeout, child.wait()).await.is_err() {
        tracing::error!(
            label,
            "the abandoned process did not exit after termination"
        );
    }
}

struct Supervised {
    pid: u32,
    label: Arc<str>,
    tail: Arc<Mutex<TailBuffer>>,
    stderr_task: Option<JoinHandle<()>>,
    ledger: Ledger,
    running: Arc<AtomicUsize>,
    kill_confirm_timeout: Duration,
    /// Tells when every process of the tree has exited.
    #[cfg(windows)]
    tree: Option<Arc<platform::TreeJob>>,
}

/// Where [`TreeAccounting`] leaves the job it created during the spawn.
#[cfg(windows)]
pub(crate) type TreeSlot = Arc<Mutex<Option<platform::TreeJob>>>;

/// Puts the new child into a [`platform::TreeJob`] while it is still suspended (process-wrap's
/// `JobObject` creates it suspended and resumes it only in `wrap_child`, which runs after every
/// `post_spawn`).
#[cfg(windows)]
#[derive(Debug)]
pub(crate) struct TreeAccounting(pub(crate) TreeSlot);

#[cfg(windows)]
impl CommandWrapper for TreeAccounting {
    fn post_spawn(
        &mut self,
        _command: &mut Command,
        child: &mut tokio::process::Child,
        _core: &CommandWrap,
    ) -> std::io::Result<()> {
        let handle = child
            .raw_handle()
            .ok_or_else(|| std::io::Error::other("the new process has no handle"))?;
        let tree = platform::TreeJob::for_process(windows::Win32::Foundation::HANDLE(handle))?;
        *self.0.lock() = Some(tree);
        Ok(())
    }
}

/// Owns the process wrapper for its whole life.
///
/// 1. Wait for the *main* process to exit, applying kill requests meanwhile.
/// 2. Terminate whatever is still alive in the job: when the agent CLI is gone its session is
///    over, and anything it started (dev servers, watchers…) must not outlive it.
/// 3. Wait until every process of the tree has exited (`TerminateJobObject` only starts the
///    termination), reap the job, drain stderr, publish [`ExitInfo`]. Whoever acts on the exit
///    (e.g. removes the worktree the tree ran in) finds no process of the tree left.
async fn supervise(
    mut child: Box<dyn ChildWrapper>,
    mut cmd_rx: mpsc::UnboundedReceiver<ChildCommand>,
    exit_tx: watch::Sender<Option<ExitInfo>>,
    s: Supervised,
) {
    let mut stopped: Option<StopReason> = None;
    let mut commands_open = true;
    let main_status = loop {
        tokio::select! {
            status = child.inner_mut().wait() => break status,
            cmd = cmd_rx.recv(), if commands_open => {
                let reason = match cmd {
                    Some(ChildCommand::Kill(reason)) => reason,
                    None => {
                        commands_open = false;
                        StopReason::Abandoned
                    }
                };
                stopped.get_or_insert(reason);
                if let Err(e) = child.start_kill() {
                    tracing::warn!(pid = s.pid, label = %s.label, error = %e, "terminating process tree failed");
                }
            }
        }
    };

    // The main process is gone: end the rest of the tree and wait until it is gone.
    if let Err(e) = child.start_kill() {
        tracing::debug!(pid = s.pid, label = %s.label, error = %e, "terminating remaining job processes failed");
    }
    #[cfg(windows)]
    let tree = match s.tree {
        Some(tree) => {
            let timeout = s.kill_confirm_timeout;
            let waiting = tree.clone();
            let drained = tokio::task::spawn_blocking(move || waiting.terminate_and_wait(timeout))
                .await
                .expect("the tree wait does not panic");
            match drained {
                Ok(true) => {}
                Ok(false) => {
                    tracing::error!(pid = s.pid, label = %s.label, "processes of the tree are still alive after termination")
                }
                Err(e) => {
                    tracing::warn!(pid = s.pid, label = %s.label, error = %e, "cannot tell whether the tree has exited")
                }
            }
            Some(tree)
        }
        None => None,
    };
    if tokio::time::timeout(s.kill_confirm_timeout, child.wait())
        .await
        .is_err()
    {
        tracing::error!(pid = s.pid, label = %s.label, "job did not drain after termination");
    }
    if let Some(task) = s.stderr_task
        && tokio::time::timeout(s.kill_confirm_timeout, task)
            .await
            .is_err()
    {
        tracing::warn!(pid = s.pid, label = %s.label, "stderr pipe stayed open after the tree exited");
    }

    let code = match &main_status {
        Ok(status) => status.code(),
        Err(e) => {
            tracing::warn!(pid = s.pid, label = %s.label, error = %e, "waiting for process failed");
            None
        }
    };
    let info = ExitInfo {
        code,
        stopped,
        stderr_tail: s.tail.lock().to_string_lossy(),
        exited_at_ms: now_ms(),
    };
    // The bookkeeping is settled before the end is announced (the log line and `ExitInfo`):
    // whoever learns that the process ended — a waiter, or a reader of the log — finds it
    // gone from the ledger and from `running_count`.
    s.ledger.remove(s.pid);
    s.running.fetch_sub(1, Ordering::SeqCst);
    tracing::info!(pid = s.pid, label = %s.label, outcome = %info.describe(), "supervised process ended");
    exit_tx.send_replace(Some(info));
    #[cfg(windows)]
    drop(tree);
}
