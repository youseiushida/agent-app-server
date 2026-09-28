//! Supervision of agent processes.
//!
//! Every child runs in its own Windows Job Object created with `KILL_ON_JOB_CLOSE`
//! (via `process-wrap`: the process is created suspended, assigned to the job, then resumed,
//! so grandchildren cannot escape). Consequences:
//!
//! * [`ChildHandle::kill`] terminates the whole tree (`TerminateJobObject`).
//! * If the daemon dies for any reason, the OS closes the job handles and kills every tree.
//! * [`ChildHandle::wait`] resolves only when every process of the job has exited (a second,
//!   accounting job around process-wrap's job tells when the tree is empty).
//!
//! In addition the supervisor keeps a ledger of spawned PIDs (with their creation time) so
//! the next start can detect and kill anything that survived, descendants included (see
//! [`Supervisor::sweep_orphans`]).

mod child;
mod ledger;
mod orphans;
mod platform;
mod power;
mod resolve;
mod tail;
mod tool;

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

pub use child::{ChildHandle, ExitInfo, ManagedChild, StopReason};
pub use ledger::{LedgerEntry, SweepReport, SweptDescendant};
pub use orphans::ProcessId;
pub use platform::{process_creation_time, process_is_running, terminate_if_same};
pub use power::{PowerGuard, PowerLease};
pub use resolve::{ResolveError, resolve_program};
pub use tool::{ToolCancel, ToolChunk, ToolError, ToolOutput, ToolSpec, ToolStream};

/// Policy values of the supervisor (see `docs/design.md` §13; the daemon builds them from
/// `[policy]`, and `Default` holds the same defaults as that table).
#[derive(Debug, Clone)]
pub struct SupervisorPolicy {
    /// Bytes of stderr kept per child for exit reports. Default 16 KiB: enough for a CLI's
    /// last error messages and a stack trace, small per process.
    pub stderr_tail_bytes: usize,
    /// Upper bound for short-lived tools (git …) when the caller gives none. Default 120 s:
    /// local git commands finish in seconds; a hung one must not block a turn forever.
    pub tool_timeout: Duration,
    /// How long to wait for the tree to disappear after `TerminateJobObject` before reporting
    /// the process as unkillable. Termination is synchronous in the kernel, so this only
    /// guards against a wedged wait. Default 10 s.
    pub kill_confirm_timeout: Duration,
    /// Keep the machine awake while a [`PowerLease`] is held. Default on: a PC that sleeps in
    /// the middle of a turn stops the agent and cannot be reached from the phone.
    pub prevent_sleep: bool,
}

impl Default for SupervisorPolicy {
    fn default() -> Self {
        Self {
            stderr_tail_bytes: 16 * 1024,
            tool_timeout: Duration::from_secs(120),
            kill_confirm_timeout: Duration::from_secs(10),
            prevent_sleep: true,
        }
    }
}

/// What to spawn.
#[derive(Debug, Clone)]
pub struct SpawnSpec {
    /// Short name used in logs and the ledger (e.g. `claude[thr_…]`).
    pub label: String,
    /// Resolved program path (see [`resolve_program`]).
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub cwd: PathBuf,
    /// Variables added to (or overriding) the inherited environment.
    pub env: Vec<(OsString, OsString)>,
    /// Variables removed from the inherited environment.
    pub env_remove: Vec<OsString>,
    /// Owner recorded in the ledger (usually a thread id).
    pub owner: Option<String>,
}

impl SpawnSpec {
    pub fn new(
        label: impl Into<String>,
        program: impl Into<PathBuf>,
        cwd: impl Into<PathBuf>,
    ) -> Self {
        Self {
            label: label.into(),
            program: program.into(),
            args: Vec::new(),
            cwd: cwd.into(),
            env: Vec::new(),
            env_remove: Vec::new(),
            owner: None,
        }
    }

    pub fn arg(mut self, arg: impl Into<OsString>) -> Self {
        self.args.push(arg.into());
        self
    }

    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    pub fn env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    pub fn env_remove(mut self, key: impl Into<OsString>) -> Self {
        self.env_remove.push(key.into());
        self
    }

    pub fn owner(mut self, owner: impl Into<String>) -> Self {
        self.owner = Some(owner.into());
        self
    }
}

/// Spawning failed.
#[derive(Debug, thiserror::Error)]
pub enum SpawnError {
    #[error("working directory {0} does not exist")]
    MissingCwd(PathBuf),
    #[error("failed to start {program}: {source}")]
    Io {
        program: String,
        #[source]
        source: std::io::Error,
    },
    /// The process started but could not be recorded in the child ledger (its identity could
    /// not be read or the ledger could not be written). It was terminated: after a daemon crash
    /// nothing would have found it (design.md §4.5).
    #[error(
        "{program} was started but could not be recorded for cleanup, so it was stopped: {source}"
    )]
    Untracked {
        program: String,
        #[source]
        source: std::io::Error,
    },
}

/// Supervisor shared by the whole daemon.
#[derive(Clone)]
pub struct Supervisor {
    inner: Arc<Inner>,
}

struct Inner {
    policy: SupervisorPolicy,
    ledger: ledger::Ledger,
    power: PowerGuard,
    running: Arc<AtomicUsize>,
}

impl Supervisor {
    /// `state_dir` holds the PID ledger (`children.json`).
    pub fn new(state_dir: &Path, policy: SupervisorPolicy) -> std::io::Result<Self> {
        std::fs::create_dir_all(state_dir)?;
        let power = PowerGuard::new(policy.prevent_sleep);
        Ok(Self {
            inner: Arc::new(Inner {
                ledger: ledger::Ledger::open(state_dir.join("children.json"))?,
                policy,
                power,
                running: Arc::new(AtomicUsize::new(0)),
            }),
        })
    }

    pub fn policy(&self) -> &SupervisorPolicy {
        &self.inner.policy
    }

    /// Kills processes recorded by a previous daemon instance that are still alive (same PID
    /// *and* same creation time), together with their descendants (see `docs/design.md`
    /// §4.5), then clears the ledger. Waits up to `kill_confirm_timeout` per recorded tree for
    /// it to be gone. Call once at startup, before spawning anything (it blocks the thread).
    pub fn sweep_orphans(&self) -> SweepReport {
        self.inner
            .ledger
            .sweep(self.inner.policy.kill_confirm_timeout)
    }

    /// Spawns a supervised child with piped stdin/stdout/stderr.
    pub async fn spawn(&self, spec: SpawnSpec) -> Result<ManagedChild, SpawnError> {
        child::spawn(
            spec,
            self.inner.policy.clone(),
            self.inner.ledger.clone(),
            self.inner.running.clone(),
        )
        .await
    }

    /// Runs a short-lived tool to completion (inside its own job).
    pub async fn run_tool(&self, spec: ToolSpec) -> Result<ToolOutput, ToolError> {
        tool::run(spec, &self.inner.policy, None, None).await
    }

    /// Runs a tool like [`run_tool`](Self::run_tool) and also sends every chunk it writes to
    /// `chunks` as soon as it is read (the complete output is still returned). `cancel`
    /// terminates the tool's whole process tree; the run then ends with
    /// [`ToolError::Cancelled`] once every process of the tree is gone.
    pub async fn run_tool_streaming(
        &self,
        spec: ToolSpec,
        chunks: tokio::sync::mpsc::UnboundedSender<ToolChunk>,
        cancel: &ToolCancel,
    ) -> Result<ToolOutput, ToolError> {
        tool::run(spec, &self.inner.policy, Some(chunks), Some(cancel)).await
    }

    /// Sleep inhibition for the duration of running turns.
    pub fn power(&self) -> &PowerGuard {
        &self.inner.power
    }

    /// Number of supervised children currently alive.
    pub fn running_count(&self) -> usize {
        self.inner.running.load(Ordering::SeqCst)
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    /// A process the ledger cannot record is stopped and reported, never left running.
    #[tokio::test]
    async fn a_spawn_that_cannot_be_recorded_fails_and_leaves_nothing_running() {
        let dir = tempfile::tempdir().unwrap();
        let supervisor = Supervisor::new(
            dir.path(),
            SupervisorPolicy {
                prevent_sleep: false,
                ..Default::default()
            },
        )
        .unwrap();
        // A directory where the ledger's temporary file goes makes every ledger write fail.
        std::fs::create_dir(dir.path().join("children.json.tmp")).unwrap();
        let cmd = resolve_program("cmd").expect("cmd.exe is on every Windows PATH");
        let marker = dir.path().join("still-running");
        let script = format!(
            "ping -n 3 127.0.0.1 >nul & echo x> \"{}\"",
            marker.display()
        );
        let Err(err) = supervisor
            .spawn(SpawnSpec::new("unrecorded", cmd, dir.path()).args([
                "/D",
                "/C",
                script.as_str(),
            ]))
            .await
        else {
            panic!("the spawn must fail");
        };
        assert!(matches!(err, SpawnError::Untracked { .. }), "{err}");
        assert_eq!(supervisor.running_count(), 0);
        // Had the process been left running, it would write its marker after ~2 s.
        tokio::time::sleep(Duration::from_secs(4)).await;
        assert!(!marker.exists(), "the unrecorded process was terminated");
        assert!(
            !dir.path().join("children.json").exists(),
            "nothing was recorded"
        );
    }

    #[tokio::test]
    async fn the_tree_contains_its_processes_and_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        let supervisor = Supervisor::new(
            dir.path(),
            SupervisorPolicy {
                prevent_sleep: false,
                ..Default::default()
            },
        )
        .unwrap();
        let cmd = resolve_program("cmd").expect("cmd.exe is on every Windows PATH");
        let child = supervisor
            .spawn(SpawnSpec::new("member", cmd, dir.path()).args([
                "/D",
                "/C",
                "ping -n 30 127.0.0.1 >nul",
            ]))
            .await
            .unwrap();
        let handle = child.handle.clone();
        let created = process_creation_time(handle.pid()).expect("the child runs");
        assert!(handle.tree_contains(handle.pid(), created).unwrap());
        assert!(
            !handle.tree_contains(handle.pid(), created + 1).unwrap(),
            "another creation time is another process"
        );
        let me = std::process::id();
        let mine = process_creation_time(me).unwrap();
        assert!(
            !handle.tree_contains(me, mine).unwrap(),
            "the test is not in the tree"
        );
        // PIDs are multiples of 4 on Windows; 3 names no process.
        assert!(!handle.tree_contains(3, 1).unwrap());
        handle.kill(StopReason::User);
        handle.wait().await;
    }
}
