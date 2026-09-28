//! Process-tree guarantees of the supervisor (design.md §4 and §16 "プロセスリークテスト").
//!
//! The trees are made of real `aas-dummy-agent` processes; each records itself (PID plus
//! creation time) in a directory, so the tests know every descendant and can check that none
//! survives. Every test holds a [`Cleanup`] guard: a failed assertion still terminates what the
//! test started, including processes that live outside any job.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use aas_supervisor::{
    ManagedChild, SpawnSpec, StopReason, Supervisor, SupervisorPolicy, ToolError, ToolSpec,
};
use aas_testkit::proc::{self, Cleanup, Proc};

/// Upper bound for a whole tree to start and record itself (generous: tests run in parallel).
const START: Duration = Duration::from_secs(60);
/// Upper bound for a terminated tree to disappear (design.md §16: "within 10 s").
const DEATH: Duration = Duration::from_secs(10);

fn supervisor(state: &Path) -> Supervisor {
    Supervisor::new(
        state,
        SupervisorPolicy {
            prevent_sleep: false,
            ..Default::default()
        },
    )
    .expect("supervisor")
}

fn dummy_agent() -> PathBuf {
    proc::bin_path("aas-dummy-agent")
}

fn tree_args(depth: u32, width: u32, pid_dir: &Path) -> Vec<String> {
    vec![
        "tree".to_owned(),
        "--depth".to_owned(),
        depth.to_string(),
        "--width".to_owned(),
        width.to_string(),
        "--pid-dir".to_owned(),
        pid_dir.display().to_string(),
    ]
}

async fn spawn(supervisor: &Supervisor, cwd: &Path, args: Vec<String>) -> ManagedChild {
    supervisor
        .spawn(SpawnSpec::new("tree", dummy_agent(), cwd).args(args))
        .await
        .expect("spawn supervised tree")
}

/// Runs blocking helper code off the runtime, re-raising its panic with the original message.
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    match tokio::task::spawn_blocking(f).await {
        Ok(v) => v,
        Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
        Err(e) => panic!("blocking helper failed: {e}"),
    }
}

async fn wait_recorded(dir: &Path, count: usize) -> Vec<Proc> {
    let dir = dir.to_owned();
    blocking(move || proc::wait_for_pids(&dir, count, START)).await
}

async fn wait_dead(procs: &[Proc]) -> Vec<Proc> {
    let procs = procs.to_vec();
    blocking(move || proc::wait_all_dead(&procs, DEATH)).await
}

fn ledger(state: &Path) -> Vec<serde_json::Value> {
    let bytes = std::fs::read(state.join("children.json")).expect("ledger file");
    serde_json::from_slice(&bytes).expect("ledger is a JSON array")
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_millis() as u64
}

/// Log lines of the whole test binary. The supervisor reports how each tree ended only through
/// its handles and its log; once every handle is dropped the log is the only witness.
fn captured_log() -> Arc<Mutex<Vec<u8>>> {
    static LOG: OnceLock<Arc<Mutex<Vec<u8>>>> = OnceLock::new();
    LOG.get_or_init(|| {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let sink = buf.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_max_level(tracing::Level::INFO)
            .with_writer(move || LogSink(sink.clone()))
            .finish();
        tracing::subscriber::set_global_default(subscriber).expect("install log capture");
        buf
    })
    .clone()
}

struct LogSink(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for LogSink {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("log buffer").extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Waits for a log line containing every fragment; returns it.
async fn wait_log_line(log: &Arc<Mutex<Vec<u8>>>, fragments: &[&str]) -> String {
    let deadline = Instant::now() + DEATH;
    loop {
        let text = String::from_utf8_lossy(&log.lock().expect("log buffer")).into_owned();
        if let Some(line) = text
            .lines()
            .find(|l| fragments.iter().all(|f| l.contains(f)))
        {
            return line.to_owned();
        }
        assert!(Instant::now() < deadline, "no log line with {fragments:?}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn staged_stop_terminates_a_tree_that_ignores_eof() {
    let dir = tempfile::tempdir().unwrap();
    let pids = dir.path().join("pids");
    let mut cleanup = Cleanup::new();
    cleanup.dir(&pids);
    let state = dir.path().join("state");
    let supervisor = supervisor(&state);

    let mut child = spawn(&supervisor, dir.path(), tree_args(2, 2, &pids)).await;
    let procs = wait_recorded(&pids, proc::tree_size(2, 2)).await;
    assert_eq!(procs.len(), 7, "root, 2 children, 4 grandchildren");
    assert!(
        procs.iter().any(|p| p.pid == child.handle.pid()),
        "the root is the supervised process"
    );
    assert_eq!(supervisor.running_count(), 1);
    assert_eq!(
        ledger(&state).len(),
        1,
        "the root is on the ledger while it runs"
    );

    // Stage 1: EOF on stdin (the tree ignores it). Stage 2: terminate the job after the grace.
    drop(child.stdin.take());
    let grace = Duration::from_millis(300);
    let started = Instant::now();
    let info = child.handle.shutdown(grace, StopReason::User).await;
    assert!(started.elapsed() >= grace, "the grace period was honoured");
    assert_eq!(info.stopped, Some(StopReason::User));
    assert!(!info.is_clean());

    assert_eq!(
        wait_dead(&procs).await,
        Vec::<Proc>::new(),
        "survivors of the staged stop"
    );
    assert_eq!(supervisor.running_count(), 0);
    assert!(ledger(&state).is_empty(), "the reaped root left the ledger");
    assert_eq!(
        child.handle.wait().await,
        info,
        "wait() reports the same outcome afterwards"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn staged_stop_lets_a_cooperative_process_exit_by_itself() {
    let dir = tempfile::tempdir().unwrap();
    let supervisor = supervisor(&dir.path().join("state"));
    let mut child = supervisor
        .spawn(SpawnSpec::new("agent", dummy_agent(), dir.path()).arg("agent"))
        .await
        .expect("spawn agent");
    let mut cleanup = Cleanup::new();
    cleanup.process(Proc::of(child.handle.pid()).expect("agent is running"));

    // The fake agent ends its session on EOF, well within the grace period.
    drop(child.stdin.take());
    let info = child.handle.shutdown(START, StopReason::User).await;
    assert_eq!(info.stopped, None, "no termination was needed");
    assert_eq!(info.code, Some(0));
    assert!(info.is_clean());
    assert_eq!(supervisor.running_count(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn dropping_every_handle_terminates_the_tree() {
    let log = captured_log();
    let dir = tempfile::tempdir().unwrap();
    let pids = dir.path().join("pids");
    let mut cleanup = Cleanup::new();
    cleanup.dir(&pids);
    let supervisor = supervisor(&dir.path().join("state"));

    let child = spawn(&supervisor, dir.path(), tree_args(1, 2, &pids)).await;
    let procs = wait_recorded(&pids, proc::tree_size(1, 2)).await;
    let root = child.handle.pid();
    let last_handle = child.handle.clone();
    drop(child);
    // One clone is enough to keep the tree: nothing may be terminated yet.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        procs.iter().all(Proc::alive),
        "a live handle keeps the tree running"
    );
    assert_eq!(supervisor.running_count(), 1);

    drop(last_handle);
    assert_eq!(
        wait_dead(&procs).await,
        Vec::<Proc>::new(),
        "survivors of an abandoned tree"
    );
    let line = wait_log_line(&log, &["supervised process ended", &format!("pid={root} ")]).await;
    assert!(
        line.contains("outcome=stopped (abandoned)"),
        "unexpected outcome: {line}"
    );
    assert_eq!(supervisor.running_count(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn descendants_of_an_exited_process_are_terminated() {
    let dir = tempfile::tempdir().unwrap();
    let pids = dir.path().join("pids");
    let mut cleanup = Cleanup::new();
    cleanup.dir(&pids);
    let supervisor = supervisor(&dir.path().join("state"));

    let args = vec![
        "linger".to_owned(),
        "--pid-dir".to_owned(),
        pids.display().to_string(),
    ];
    let child = spawn(&supervisor, dir.path(), args).await;
    let info = tokio::time::timeout(START, child.handle.wait())
        .await
        .expect("wait() resolves once the main process exits");
    assert_eq!(info.code, Some(0), "stderr: {}", info.stderr_tail);
    assert_eq!(info.stopped, None, "the main process exited by itself");
    // `linger` exits only after its child recorded itself: both are on record.
    let procs = proc::recorded(&pids);
    assert_eq!(procs.len(), 2, "{procs:?}");
    assert_eq!(
        wait_dead(&procs).await,
        Vec::<Proc>::new(),
        "the lingering child outlived its parent"
    );
    assert_eq!(supervisor.running_count(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_tree_has_exited_when_wait_returns() {
    // `wait()` must not resolve while a process of the tree is still being torn down
    // (`TerminateJobObject` only starts the termination): callers act on the exit right away,
    // e.g. `git worktree remove` of the folder the tree ran in.
    let dir = tempfile::tempdir().unwrap();
    let mut cleanup = Cleanup::new();
    let supervisor = supervisor(&dir.path().join("state"));
    for round in 0..5 {
        let pids = dir.path().join(format!("pids{round}"));
        cleanup.dir(&pids);
        let work = dir.path().join(format!("work{round}"));
        std::fs::create_dir_all(&work).unwrap();
        // Every process of the tree runs in `work`, so each holds that folder open until it exits.
        let child = spawn(&supervisor, &work, tree_args(2, 3, &pids)).await;
        let procs = wait_recorded(&pids, proc::tree_size(2, 3)).await;
        let exited = if round % 2 == 0 {
            // The tree is stopped...
            child.handle.kill(StopReason::User);
            tokio::time::timeout(START, child.handle.wait())
                .await
                .expect("wait() resolves")
        } else {
            // ...or its main process ends by itself and the rest is terminated.
            let root = procs
                .iter()
                .copied()
                .find(|p| p.pid == child.handle.pid())
                .expect("root recorded");
            assert!(root.terminate());
            tokio::time::timeout(START, child.handle.wait())
                .await
                .expect("wait() resolves")
        };
        assert!(exited.code.is_some() || exited.stopped.is_some());
        let running: Vec<Proc> = procs.iter().copied().filter(Proc::alive).collect();
        assert_eq!(
            running,
            Vec::<Proc>::new(),
            "round {round}: wait() returned while the tree was alive"
        );
        std::fs::remove_dir_all(&work)
            .unwrap_or_else(|e| panic!("round {round}: the tree still holds its folder: {e}"));
    }
    assert_eq!(supervisor.running_count(), 0);
}

/// Kills the host (and waits for it) when dropped.
struct Host(Child);

impl Drop for Host {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn killing_the_daemon_kills_its_supervised_trees() {
    let dir = tempfile::tempdir().unwrap();
    let pids = dir.path().join("pids");
    let state = dir.path().join("state");
    let mut cleanup = Cleanup::new();
    cleanup.dir(&pids);

    let mut host = Host(
        Command::new(proc::bin_path("aas-supervisor-host"))
            .arg(&state)
            .arg(&pids)
            .args(["2", "2"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("start aas-supervisor-host"),
    );
    let stdout = host.0.stdout.take().expect("host stdout");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let deadline = Instant::now() + START;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(line) if line.trim() == "ready" => break,
            Ok(_) => continue,
            Err(e) => panic!("the host never became ready: {e}"),
        }
    }
    let procs = proc::recorded(&pids);
    assert_eq!(procs.len(), proc::tree_size(2, 2), "{procs:?}");

    // TerminateProcess on the daemon: the OS closes its job handles, KILL_ON_JOB_CLOSE does the rest.
    host.0.kill().expect("kill host");
    host.0.wait().expect("reap host");
    let survivors = proc::wait_all_dead(&procs, DEATH);
    assert!(
        survivors.is_empty(),
        "processes outlived the killed daemon: {survivors:?}"
    );

    // The dead daemon's ledger still names the root, but the process is gone: the next start
    // has nothing to terminate.
    let report = supervisor(&state).sweep_orphans();
    assert!(report.terminated.is_empty(), "{report:?}");
    assert!(report.failed.is_empty(), "{report:?}");
    assert_eq!(report.already_gone, 1);
    assert!(ledger(&state).is_empty(), "the sweep clears the ledger");
}

#[tokio::test(flavor = "multi_thread")]
async fn breakaway_from_the_job_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let pids = dir.path().join("pids");
    let escaped = pids.join("escaped");
    let mut cleanup = Cleanup::new();
    cleanup.dir(&pids);
    cleanup.dir(&escaped);
    let supervisor = supervisor(&dir.path().join("state"));

    let mut args = tree_args(0, 0, &pids);
    args.push("--try-breakaway".to_owned());
    let child = spawn(&supervisor, dir.path(), args).await;
    let root = wait_recorded(&pids, 1).await;
    let (refused, allowed) = (pids.join("breakaway-refused"), pids.join("breakaway-ok"));
    let deadline = Instant::now() + START;
    while !refused.exists() && !allowed.exists() {
        assert!(
            Instant::now() < deadline,
            "the root never reported its breakaway attempt"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        !allowed.exists(),
        "CREATE_BREAKAWAY_FROM_JOB succeeded: the job allows breakaway"
    );
    assert!(refused.exists());
    assert!(
        proc::recorded(&escaped).is_empty(),
        "a process escaped the job"
    );

    child.handle.kill(StopReason::User);
    child.handle.wait().await;
    assert_eq!(wait_dead(&root).await, Vec::<Proc>::new());
}

/// Starts `aas-dummy-agent tree --depth 0` outside any supervisor job and returns it once it
/// has recorded itself. The `Child` is dropped (closing our process handle) so that liveness
/// checks see the process disappear as soon as it is terminated.
fn spawn_unsupervised(pid_dir: &Path) -> Proc {
    let child = Command::new(dummy_agent())
        .args(tree_args(0, 0, pid_dir))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn unsupervised process");
    let pid = child.id();
    drop(child);
    let procs = proc::wait_for_pids(pid_dir, 1, START);
    assert_eq!(procs[0].pid, pid);
    procs[0]
}

#[test]
fn sweep_terminates_ledger_survivors_and_spares_recycled_pids() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    std::fs::create_dir_all(&state).unwrap();
    let (orphan_dir, bystander_dir) = (dir.path().join("orphan"), dir.path().join("bystander"));
    let mut cleanup = Cleanup::new();
    cleanup.dir(&orphan_dir);
    cleanup.dir(&bystander_dir);

    // A survivor of a crashed daemon (on the ledger with its true creation time), and an
    // unrelated process that happens to carry a PID from the ledger (different creation time).
    let orphan = spawn_unsupervised(&orphan_dir);
    let bystander = spawn_unsupervised(&bystander_dir);
    let created = aas_supervisor::process_creation_time(orphan.pid).expect("orphan is running");
    assert_eq!(created, orphan.created);
    let entries = serde_json::json!([
        {"pid": orphan.pid, "created": created, "label": "fake[thr_orphan]", "owner": null, "spawned_at_ms": now_ms()},
        {"pid": bystander.pid, "created": bystander.created + 1, "label": "fake[thr_old]", "owner": null, "spawned_at_ms": now_ms()},
    ]);
    std::fs::write(
        state.join("children.json"),
        serde_json::to_vec_pretty(&entries).unwrap(),
    )
    .unwrap();

    let report = supervisor(&state).sweep_orphans();
    let terminated: Vec<u32> = report.terminated.iter().map(|e| e.pid).collect();
    assert_eq!(terminated, vec![orphan.pid], "{report:?}");
    assert_eq!(report.terminated[0].label, "fake[thr_orphan]");
    assert!(
        report.descendants.is_empty(),
        "the orphan had no children: {report:?}"
    );
    assert_eq!(report.already_gone, 1, "the recycled PID counts as gone");
    assert!(report.failed.is_empty(), "{report:?}");
    assert!(
        proc::wait_all_dead(&[orphan], DEATH).is_empty(),
        "the orphan survived the sweep"
    );
    assert!(
        bystander.alive(),
        "a process whose creation time does not match must not be touched"
    );
    assert!(ledger(&state).is_empty(), "the sweep clears the ledger");

    // A second start finds nothing.
    let again = supervisor(&state).sweep_orphans();
    assert_eq!(again, aas_supervisor::SweepReport::default());
    assert!(bystander.terminate());
}

/// Writes a ledger naming `root` (PID and creation time), as a crashed daemon leaves it.
fn write_ledger(state: &Path, root: Proc, label: &str) {
    std::fs::create_dir_all(state).unwrap();
    let entries = serde_json::json!([
        {"pid": root.pid, "created": root.created, "label": label, "owner": null, "spawned_at_ms": now_ms()},
    ]);
    std::fs::write(
        state.join("children.json"),
        serde_json::to_vec_pretty(&entries).unwrap(),
    )
    .unwrap();
}

#[test]
fn sweep_terminates_the_descendants_of_a_ledger_survivor() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let (tree_dir, bystander_dir) = (dir.path().join("tree"), dir.path().join("bystander"));
    let mut cleanup = Cleanup::new();
    cleanup.dir(&tree_dir);
    cleanup.dir(&bystander_dir);

    // A whole tree that lives outside any supervisor job (as if it had escaped), plus an
    // unrelated process started afterwards.
    let root_child = Command::new(dummy_agent())
        .args(tree_args(2, 2, &tree_dir))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn unsupervised tree");
    let root_pid = root_child.id();
    drop(root_child);
    let tree = proc::wait_for_pids(&tree_dir, proc::tree_size(2, 2), START);
    let root = tree
        .iter()
        .copied()
        .find(|p| p.pid == root_pid)
        .expect("root recorded");
    let bystander = spawn_unsupervised(&bystander_dir);
    write_ledger(&state, root, "fake[thr_tree]");

    let report = supervisor(&state).sweep_orphans();
    assert!(report.failed.is_empty(), "{report:?}");
    let terminated: Vec<u32> = report.terminated.iter().map(|e| e.pid).collect();
    assert_eq!(terminated, vec![root.pid], "{report:?}");
    let mut swept: Vec<Proc> = report
        .descendants
        .iter()
        .map(|d| Proc {
            pid: d.process.pid,
            created: d.process.created,
        })
        .collect();
    swept.sort_unstable();
    let mut expected: Vec<Proc> = tree.iter().copied().filter(|p| p.pid != root.pid).collect();
    expected.sort_unstable();
    assert_eq!(
        swept, expected,
        "every descendant (and nothing else) was attributed to the root"
    );
    assert!(report.descendants.iter().all(|d| d.root_pid == root.pid));
    // `sweep_orphans` returns once the tree is confirmed gone.
    let alive: Vec<Proc> = tree.iter().copied().filter(Proc::alive).collect();
    assert_eq!(alive, Vec::<Proc>::new(), "survivors of the sweep");
    assert!(
        bystander.alive(),
        "an unrelated process must not be touched"
    );
    assert!(ledger(&state).is_empty(), "the sweep clears the ledger");
}

#[test]
fn sweep_attributes_descendants_only_through_a_parent_that_still_matches() {
    // `linger` exits after starting a child: the ledger names a process that is gone, so the
    // child cannot be proven to be its descendant (its parent PID may have been recycled) and
    // is left alone (docs/design.md §4.5).
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let pids = dir.path().join("pids");
    let mut cleanup = Cleanup::new();
    cleanup.dir(&pids);

    let mut parent = Command::new(dummy_agent())
        .args([
            "linger".to_owned(),
            "--pid-dir".to_owned(),
            pids.display().to_string(),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn linger");
    let parent_pid = parent.id();
    let status = parent.wait().expect("linger exits");
    assert!(status.success());
    // Our handle would keep the exited process object (and its PID) alive; without it the
    // PID is free for reuse, as after a real crash.
    drop(parent);
    let procs = proc::recorded(&pids);
    assert_eq!(procs.len(), 2, "{procs:?}");
    let recorded_parent = procs
        .iter()
        .copied()
        .find(|p| p.pid == parent_pid)
        .expect("parent recorded");
    let child = procs
        .iter()
        .copied()
        .find(|p| p.pid != parent_pid)
        .expect("child recorded");
    write_ledger(&state, recorded_parent, "fake[thr_linger]");

    let report = supervisor(&state).sweep_orphans();
    assert!(report.terminated.is_empty(), "{report:?}");
    assert!(report.descendants.is_empty(), "{report:?}");
    assert_eq!(report.already_gone, 1);
    assert!(
        child.alive(),
        "a process whose parent is gone is never attributed"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn tool_timeout_terminates_the_tool_tree() {
    let dir = tempfile::tempdir().unwrap();
    let pids = dir.path().join("pids");
    let mut cleanup = Cleanup::new();
    cleanup.dir(&pids);
    let supervisor = supervisor(&dir.path().join("state"));

    let timeout = Duration::from_secs(8);
    let started = Instant::now();
    let err = supervisor
        .run_tool(
            ToolSpec::new(dummy_agent(), dir.path())
                .args(tree_args(1, 2, &pids))
                .timeout(timeout),
        )
        .await
        .expect_err("the tree never exits by itself");
    assert!(
        matches!(err, ToolError::Timeout { timeout: t, .. } if t == timeout),
        "{err}"
    );
    assert!(started.elapsed() >= timeout);
    let procs = proc::recorded(&pids);
    assert_eq!(
        procs.len(),
        proc::tree_size(1, 2),
        "the whole tree was running when the timeout hit: {procs:?}"
    );
    // Not polled: the run returns only once the whole tree is gone (design.md §4.8).
    let running: Vec<Proc> = procs.iter().copied().filter(Proc::alive).collect();
    assert_eq!(
        running,
        Vec::<Proc>::new(),
        "the timed-out run returned while its tree was alive"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cancelled_tool_has_exited_when_the_run_returns() {
    // As `the_tree_has_exited_when_wait_returns`, for tools: the caller acts on the end of the
    // run right away, e.g. removes the partial folder of a cancelled `git clone`, which the
    // tool's processes hold open until they have exited.
    let dir = tempfile::tempdir().unwrap();
    let mut cleanup = Cleanup::new();
    let supervisor = supervisor(&dir.path().join("state"));
    for round in 0..3 {
        let pids = dir.path().join(format!("pids{round}"));
        cleanup.dir(&pids);
        let work = dir.path().join(format!("work{round}"));
        std::fs::create_dir_all(&work).unwrap();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let cancel = aas_supervisor::ToolCancel::new();
        let run = {
            let (supervisor, cancel, work, args) = (
                supervisor.clone(),
                cancel.clone(),
                work.clone(),
                tree_args(2, 3, &pids),
            );
            // A timeout far beyond the test: only the cancellation may end this run.
            tokio::spawn(async move {
                supervisor
                    .run_tool_streaming(
                        ToolSpec::new(dummy_agent(), work)
                            .args(args)
                            .timeout(Duration::from_secs(600)),
                        tx,
                        &cancel,
                    )
                    .await
            })
        };
        let procs = wait_recorded(&pids, proc::tree_size(2, 3)).await;
        cancel.cancel();
        let err = tokio::time::timeout(DEATH, run)
            .await
            .expect("the cancelled run ends")
            .unwrap()
            .expect_err("cancelled");
        assert!(matches!(err, ToolError::Cancelled { .. }), "{err}");
        let running: Vec<Proc> = procs.iter().copied().filter(Proc::alive).collect();
        assert_eq!(
            running,
            Vec::<Proc>::new(),
            "round {round}: the run returned while the tool's tree was alive"
        );
        std::fs::remove_dir_all(&work).unwrap_or_else(|e| {
            panic!("round {round}: the tool's tree still holds its folder: {e}")
        });
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_streaming_tool_delivers_output_while_running_and_cancel_terminates_its_tree() {
    let dir = tempfile::tempdir().unwrap();
    let pids = dir.path().join("pids");
    let mut cleanup = Cleanup::new();
    cleanup.dir(&pids);
    let supervisor = supervisor(&dir.path().join("state"));

    let mut args = tree_args(1, 2, &pids);
    args.extend(["--say".to_owned(), "Receiving objects:  42%\r".to_owned()]);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let cancel = aas_supervisor::ToolCancel::new();
    let run = {
        let (supervisor, cancel, cwd) =
            (supervisor.clone(), cancel.clone(), dir.path().to_path_buf());
        // A timeout far beyond the test: only the cancellation may end this run.
        tokio::spawn(async move {
            supervisor
                .run_tool_streaming(
                    ToolSpec::new(dummy_agent(), cwd)
                        .args(args)
                        .timeout(Duration::from_secs(600)),
                    tx,
                    &cancel,
                )
                .await
        })
    };
    // The output arrives while the tool still runs (it never exits by itself).
    let mut seen: Vec<(aas_supervisor::ToolStream, Vec<u8>)> = Vec::new();
    let deadline = tokio::time::Instant::now() + START;
    while !([
        aas_supervisor::ToolStream::Stdout,
        aas_supervisor::ToolStream::Stderr,
    ]
    .iter()
    .all(|s| {
        let bytes: Vec<u8> = seen
            .iter()
            .filter(|(st, _)| st == s)
            .flat_map(|(_, b)| b.clone())
            .collect();
        bytes == b"Receiving objects:  42%\r"
    })) {
        let chunk = tokio::time::timeout_at(deadline, rx.recv())
            .await
            .expect("output while running")
            .expect("chunk");
        seen.push((chunk.stream, chunk.bytes));
    }
    assert!(!run.is_finished(), "the tool is still running");
    let procs = wait_recorded(&pids, proc::tree_size(1, 2)).await;

    let started = Instant::now();
    cancel.cancel();
    let err = tokio::time::timeout(DEATH, run)
        .await
        .expect("the cancelled run ends")
        .unwrap()
        .expect_err("cancelled");
    assert!(matches!(err, ToolError::Cancelled { .. }), "{err}");
    assert!(started.elapsed() < DEATH);
    assert_eq!(
        wait_dead(&procs).await,
        Vec::<Proc>::new(),
        "survivors of the cancelled tool"
    );
}
