//! Short-lived tool invocations (git, …), each inside its own job.
//!
//! Two modes share one implementation:
//! * collected ([`Supervisor::run_tool`](crate::Supervisor::run_tool)): the output is returned
//!   when the tool has exited;
//! * streaming ([`Supervisor::run_tool_streaming`](crate::Supervisor::run_tool_streaming)): in
//!   addition, every chunk the tool writes is delivered while it runs, and the run can be
//!   cancelled (the tool's whole process tree is terminated through its job).

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use process_wrap::tokio::*;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::sync::{mpsc, watch};

use crate::SupervisorPolicy;

/// Size of one read from a tool's stdout / stderr pipe (the largest chunk delivered).
const READ_CHUNK_BYTES: usize = 8 * 1024;

/// A tool invocation.
#[derive(Debug, Clone)]
pub struct ToolSpec {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub cwd: PathBuf,
    pub env: Vec<(OsString, OsString)>,
    /// Bytes written to stdin (then closed). `None` gives the tool a null stdin, so it can never
    /// wait for input.
    pub stdin: Option<Vec<u8>>,
    /// Overrides the policy's `tool_timeout`.
    pub timeout: Option<Duration>,
}

impl ToolSpec {
    pub fn new(program: impl Into<PathBuf>, cwd: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            cwd: cwd.into(),
            env: Vec::new(),
            stdin: None,
            timeout: None,
        }
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

    pub fn stdin(mut self, bytes: Vec<u8>) -> Self {
        self.stdin = Some(bytes);
        self
    }

    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }
}

/// Collected result of a tool run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutput {
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl ToolOutput {
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }

    pub fn stdout_lossy(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    pub fn stderr_lossy(&self) -> String {
        String::from_utf8_lossy(&self.stderr).into_owned()
    }
}

/// The pipe a chunk of output was read from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolStream {
    Stdout,
    Stderr,
}

/// Output delivered while a tool runs (streaming mode). Chunks are raw bytes exactly as read
/// from the pipe: a chunk may end in the middle of a line or of a UTF-8 sequence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolChunk {
    pub stream: ToolStream,
    pub bytes: Vec<u8>,
}

/// Cancels a streaming tool run: the tool's whole process tree is terminated (its job), and the
/// run ends with [`ToolError::Cancelled`] once the tree is gone. Cheap to clone; cancelling is
/// idempotent, and a run started with an already cancelled token does not spawn anything.
#[derive(Debug, Clone)]
pub struct ToolCancel {
    flag: Arc<watch::Sender<bool>>,
}

impl Default for ToolCancel {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolCancel {
    pub fn new() -> Self {
        Self {
            flag: Arc::new(watch::channel(false).0),
        }
    }

    pub fn cancel(&self) {
        self.flag.send_replace(true);
    }

    pub fn is_cancelled(&self) -> bool {
        *self.flag.borrow()
    }

    /// Resolves once [`cancel`](Self::cancel) has been called.
    pub async fn cancelled(&self) {
        let mut rx = self.flag.subscribe();
        // The sender lives in `self`, so the channel cannot close while this waits.
        let _ = rx.wait_for(|c| *c).await;
    }
}

/// Tool invocation failed before producing an exit status.
#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("failed to start {program}: {source}")]
    Spawn {
        program: String,
        #[source]
        source: std::io::Error,
    },
    #[error("{program} did not finish within {timeout:?}")]
    Timeout { program: String, timeout: Duration },
    #[error("{program} was cancelled")]
    Cancelled { program: String },
    #[error("{program} was terminated but its processes did not exit within {timeout:?}")]
    KillUnconfirmed { program: String, timeout: Duration },
    #[error("i/o error while running {program}: {source}")]
    Io {
        program: String,
        #[source]
        source: std::io::Error,
    },
}

/// Reads a pipe to its end, collecting everything and forwarding each chunk to `sink`.
async fn drain<R: AsyncRead + Unpin>(
    pipe: Option<R>,
    stream: ToolStream,
    sink: Option<mpsc::UnboundedSender<ToolChunk>>,
) -> std::io::Result<Vec<u8>> {
    let mut collected = Vec::new();
    let Some(mut pipe) = pipe else {
        return Ok(collected);
    };
    let mut buf = vec![0u8; READ_CHUNK_BYTES];
    loop {
        let n = pipe.read(&mut buf).await?;
        if n == 0 {
            return Ok(collected);
        }
        collected.extend_from_slice(&buf[..n]);
        if let Some(sink) = &sink {
            // A receiver that went away no longer wants chunks; the output is still collected.
            let _ = sink.send(ToolChunk {
                stream,
                bytes: buf[..n].to_vec(),
            });
        }
    }
}

/// Knows when every process of a tool's tree is gone.
///
/// process-wrap's `wait` returns once the *main* process has exited (design.md §4.4); the rest
/// of the tree (e.g. `git index-pack` and `git-remote-https` under a cancelled `git clone`) may
/// still be running and hold files open. On Windows the tool is also put into a
/// [`platform::TreeJob`] while it is still suspended (the same accounting job supervised
/// processes get), which knows every process of the tree
/// ([`TreeJob::terminate_and_wait`](crate::platform::TreeJob::terminate_and_wait)).
struct ToolTree {
    #[cfg(windows)]
    job: Option<crate::platform::TreeJob>,
}

impl ToolTree {
    /// Terminates the whole tree, then waits until every process of it has exited and the
    /// main process is reaped, up to `timeout`. `Ok(false)`: processes were still alive when it
    /// expired.
    async fn terminate_and_wait(
        &mut self,
        child: &mut Box<dyn ChildWrapper>,
        timeout: Duration,
    ) -> std::io::Result<bool> {
        let deadline = tokio::time::Instant::now() + timeout;
        child.start_kill()?;
        #[cfg(windows)]
        if let Some(job) = self.job.take() {
            let (job, exited) = tokio::task::spawn_blocking(move || {
                let exited = job.terminate_and_wait(timeout);
                (job, exited)
            })
            .await
            .map_err(std::io::Error::other)?;
            self.job = Some(job);
            if !exited? {
                return Ok(false);
            }
        }
        match tokio::time::timeout_at(deadline, child.wait()).await {
            Ok(Ok(_)) => Ok(true),
            Ok(Err(e)) => Err(e),
            Err(_) => Ok(false),
        }
    }

    /// Whether processes of the tree are still alive after its main process exited (only
    /// known on Windows; elsewhere the process group is killed when the wrapper drops).
    fn has_leftovers(&self) -> std::io::Result<bool> {
        #[cfg(windows)]
        if let Some(job) = &self.job {
            return Ok(job.active_processes()? > 0);
        }
        Ok(false)
    }
}

/// Terminates the tool's tree and waits (bounded) until every process of it is gone.
async fn terminate(
    child: &mut Box<dyn ChildWrapper>,
    tree: &mut ToolTree,
    program: &str,
    policy: &SupervisorPolicy,
) -> Result<(), ToolError> {
    match tree
        .terminate_and_wait(child, policy.kill_confirm_timeout)
        .await
    {
        Ok(true) => Ok(()),
        Ok(false) => Err(ToolError::KillUnconfirmed {
            program: program.to_owned(),
            timeout: policy.kill_confirm_timeout,
        }),
        Err(source) => Err(ToolError::Io {
            program: program.to_owned(),
            source,
        }),
    }
}

pub(crate) async fn run(
    spec: ToolSpec,
    policy: &SupervisorPolicy,
    sink: Option<mpsc::UnboundedSender<ToolChunk>>,
    cancel: Option<&ToolCancel>,
) -> Result<ToolOutput, ToolError> {
    let program = spec.program.display().to_string();
    if cancel.is_some_and(ToolCancel::is_cancelled) {
        return Err(ToolError::Cancelled { program });
    }
    let timeout = spec.timeout.unwrap_or(policy.tool_timeout);
    let mut cmd = Command::new(&spec.program);
    cmd.args(&spec.args)
        .current_dir(&spec.cwd)
        .stdin(if spec.stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in &spec.env {
        cmd.env(k, v);
    }
    let mut wrap = CommandWrap::from(cmd);
    #[cfg(windows)]
    let tree_slot = crate::child::TreeSlot::default();
    #[cfg(windows)]
    {
        wrap.wrap(JobObject)
            .wrap(KillOnDrop)
            .wrap(CreationFlags(
                windows::Win32::System::Threading::CREATE_NO_WINDOW,
            ))
            .wrap(crate::child::TreeAccounting(tree_slot.clone()));
    }
    #[cfg(unix)]
    {
        wrap.wrap(ProcessGroup::leader()).wrap(KillOnDrop);
    }
    let mut child = wrap.spawn().map_err(|source| ToolError::Spawn {
        program: program.clone(),
        source,
    })?;
    let mut tree = ToolTree {
        #[cfg(windows)]
        job: tree_slot.lock().take(),
    };

    let stdin_task = match (child.stdin().take(), spec.stdin) {
        (Some(mut pipe), Some(bytes)) => Some(tokio::spawn(async move {
            // A tool that exits without reading all of its input closes the pipe; its exit
            // status reports the outcome.
            let _ = pipe.write_all(&bytes).await;
            let _ = pipe.shutdown().await;
        })),
        _ => None,
    };
    let stdout = child.stdout().take();
    let stderr = child.stderr().take();

    /// stdout, stderr and exit status of a tool that ran to its end.
    type Collected = (Vec<u8>, Vec<u8>, std::process::ExitStatus);
    enum Outcome {
        Finished(Result<std::io::Result<Collected>, tokio::time::error::Elapsed>),
        Cancelled,
    }
    let outcome = {
        let collect = async {
            let (out, err) = tokio::try_join!(
                drain(stdout, ToolStream::Stdout, sink.clone()),
                drain(stderr, ToolStream::Stderr, sink.clone())
            )?;
            let status = child.wait().await?;
            Ok::<_, std::io::Error>((out, err, status))
        };
        let cancelled = async {
            match cancel {
                Some(c) => c.cancelled().await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            r = tokio::time::timeout(timeout, collect) => Outcome::Finished(r),
            _ = cancelled => Outcome::Cancelled,
        }
    };

    if let Some(task) = stdin_task {
        task.abort();
    }
    // Whatever the outcome, the call returns only once no process of the tool's tree is left:
    // the caller may remove the folder the tool worked in right away.
    match outcome {
        Outcome::Finished(Ok(Ok((stdout, stderr, status)))) => {
            let leftovers = tree.has_leftovers().map_err(|source| ToolError::Io {
                program: program.clone(),
                source,
            })?;
            if leftovers {
                // The tool exited and left processes behind: they end with it.
                if let Err(e) = terminate(&mut child, &mut tree, &program, policy).await {
                    // The tool's own result stands; what it left behind still ends when the
                    // job closes with this run.
                    tracing::error!(program = %program, error = %e, "processes a finished tool left behind did not exit in time");
                }
            }
            Ok(ToolOutput {
                code: status.code(),
                stdout,
                stderr,
            })
        }
        Outcome::Finished(Ok(Err(source))) => {
            terminate(&mut child, &mut tree, &program, policy).await?;
            Err(ToolError::Io { program, source })
        }
        Outcome::Finished(Err(_)) => {
            terminate(&mut child, &mut tree, &program, policy).await?;
            Err(ToolError::Timeout { program, timeout })
        }
        Outcome::Cancelled => {
            terminate(&mut child, &mut tree, &program, policy).await?;
            Err(ToolError::Cancelled { program })
        }
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use crate::Supervisor;

    fn supervisor(dir: &std::path::Path) -> Supervisor {
        Supervisor::new(
            dir,
            SupervisorPolicy {
                prevent_sleep: false,
                ..Default::default()
            },
        )
        .unwrap()
    }

    fn cmd() -> PathBuf {
        crate::resolve_program("cmd").expect("cmd.exe is on every Windows PATH")
    }

    #[tokio::test]
    async fn streaming_delivers_every_byte_that_is_also_collected() {
        let dir = tempfile::tempdir().unwrap();
        let sup = supervisor(dir.path());
        let (tx, mut rx) = mpsc::unbounded_channel();
        let out = sup
            .run_tool_streaming(
                ToolSpec::new(cmd(), dir.path()).args([
                    "/D",
                    "/C",
                    "echo out-line& echo err-line 1>&2",
                ]),
                tx,
                &ToolCancel::new(),
            )
            .await
            .unwrap();
        assert!(out.success());
        let (mut streamed_out, mut streamed_err) = (Vec::new(), Vec::new());
        while let Ok(chunk) = rx.try_recv() {
            match chunk.stream {
                ToolStream::Stdout => streamed_out.extend(chunk.bytes),
                ToolStream::Stderr => streamed_err.extend(chunk.bytes),
            }
        }
        assert_eq!(streamed_out, out.stdout);
        assert_eq!(streamed_err, out.stderr);
        assert!(out.stdout_lossy().contains("out-line"), "{out:?}");
        assert!(out.stderr_lossy().contains("err-line"), "{out:?}");
    }

    #[tokio::test]
    async fn a_cancelled_token_prevents_the_spawn() {
        let dir = tempfile::tempdir().unwrap();
        let sup = supervisor(dir.path());
        let cancel = ToolCancel::new();
        cancel.cancel();
        let (tx, _rx) = mpsc::unbounded_channel();
        let marker = dir.path().join("ran");
        let script = format!("echo x> \"{}\"", marker.display());
        let err = sup
            .run_tool_streaming(
                ToolSpec::new(cmd(), dir.path()).args(["/D", "/C", script.as_str()]),
                tx,
                &cancel,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Cancelled { .. }), "{err}");
        assert!(!marker.exists(), "nothing was started");
    }
}
