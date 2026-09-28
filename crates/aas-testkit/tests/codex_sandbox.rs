//! Codex's Windows sandbox inside the supervisor's Job Object (design.md §4.1, §4.9).
//!
//! Opt-in and live, but it spends NO model tokens: `codex sandbox` only runs the given command
//! in Codex's sandbox; nothing talks to a model. It needs the Codex CLI (`codex` on PATH, or the
//! command in `AAS_CODEX_COMMAND`):
//!
//! ```text
//! AAS_LIVE_TESTS=1 cargo test -p aas-testkit --test codex_sandbox -- --ignored
//! ```
//!
//! The Codex adapter runs `codex app-server` under [`Supervisor::spawn`]: a Job Object with
//! `KILL_ON_JOB_CLOSE` that does not allow breakaway. Codex runs the agent's commands in its own
//! sandbox, which on Windows creates a job of its own (nested in ours) and restricted
//! processes. `codex sandbox` runs a command through that same sandbox, so for every sandbox
//! mode the adapter's permission modes request, the probe (`aas-dummy-agent job-probe`) runs as
//! `codex sandbox -c sandbox_mode=<mode> -- <probe>` under the supervisor, and the tests check:
//!
//! * the sandbox works inside our job: the probe runs and reports, and the mode really
//!   applies (a read-only sandbox cannot write to the workspace, the others can);
//! * nothing leaves our job: the probe, its child, and the process it tries to start outside
//!   its job (`CREATE_BREAKAWAY_FROM_JOB`) all belong to the supervised tree
//!   ([`aas_supervisor::ChildHandle::tree_contains`]). Codex's own job may allow breakaway (the
//!   start then succeeds), ours does not, so the process stays in ours;
//! * terminating the tree (`ChildHandle::kill`) ends Codex, the probe and the probe's children;
//! * killing the supervising process (a daemon crash) ends them too (`KILL_ON_JOB_CLOSE`).
//!
//! The Windows sandbox implementation is the one the user's Codex configuration selects
//! (`[windows] sandbox`), as for the adapter; `AAS_CODEX_WINDOWS_SANDBOX` overrides it
//! (`-c windows.sandbox=<value>`, e.g. `unelevated` or `elevated`; the elevated sandbox needs
//! Codex's one-time administrator setup).

#![cfg(windows)]

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use aas_supervisor::{SpawnSpec, StopReason, Supervisor, SupervisorPolicy, resolve_program};
use aas_testkit::proc::{self, Cleanup, Proc};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, BufReader as AsyncBufReader};

/// Upper bound for Codex to start the sandbox and the probe to report (Node.js start, sandbox
/// setup, a cold cache).
const PROBE_TIMEOUT: Duration = Duration::from_secs(120);
/// Upper bound for a terminated tree to disappear.
const DEATH: Duration = Duration::from_secs(10);

fn live_enabled() -> bool {
    std::env::var_os("AAS_LIVE_TESTS").is_some()
}

fn codex() -> PathBuf {
    let command = std::env::var("AAS_CODEX_COMMAND").unwrap_or_else(|_| "codex".to_owned());
    resolve_program(&command).unwrap_or_else(|e| panic!("the Codex CLI is needed: {e}"))
}

/// `codex sandbox` arguments that run the probe in `mode`. The command runs in Codex's working
/// folder (the spawn's cwd): `--cd` would also require a named permission profile.
fn sandbox_args(mode: &str) -> Vec<String> {
    let mut args = vec!["sandbox".to_owned()];
    if let Ok(windows) = std::env::var("AAS_CODEX_WINDOWS_SANDBOX") {
        args.extend(["-c".to_owned(), format!("windows.sandbox={windows}")]);
    }
    args.extend([
        "-c".to_owned(),
        format!("sandbox_mode={mode}"),
        "--".to_owned(),
        proc::bin_path("aas-dummy-agent").display().to_string(),
        "job-probe".to_owned(),
    ]);
    args
}

/// What the probe reported.
#[derive(Debug)]
struct Probe {
    report: Value,
    /// The probe and the processes it started.
    procs: Vec<Proc>,
}

impl Probe {
    fn parse(line: &str) -> Self {
        let report: Value =
            serde_json::from_str(line).unwrap_or_else(|e| panic!("probe line {line:?}: {e}"));
        let procs = ["probe", "child", "escaped"]
            .iter()
            .filter_map(|key| {
                let p = &report[*key];
                Some(Proc {
                    pid: p["pid"].as_u64()? as u32,
                    created: p["created"].as_u64()?,
                })
            })
            .collect();
        Self { report, procs }
    }

    /// Checks what holds for every mode, and the mode's own write policy.
    fn check(&self, mode: &str) {
        let r = &self.report;
        assert!(r["probe"].is_object(), "{mode}: {r}");
        assert!(
            r["child"].is_object(),
            "{mode}: the probe starts a child: {r}"
        );
        let wrote = r["wrote"].as_bool().expect("wrote");
        assert_eq!(
            wrote,
            mode != "read-only",
            "{mode}: the sandbox applies the mode's write policy: {r}"
        );
        eprintln!(
            "codex sandbox {mode}: breakaway {} ({})",
            r["breakaway"], r["breakawayError"]
        );
    }
}

async fn wait_dead(procs: &[Proc]) -> Vec<Proc> {
    let procs = procs.to_vec();
    tokio::task::spawn_blocking(move || proc::wait_all_dead(&procs, DEATH))
        .await
        .expect("wait")
}

#[tokio::test]
#[ignore = "runs the Codex CLI's sandbox (no model tokens); set AAS_LIVE_TESTS=1 and pass --ignored"]
async fn codex_sandboxed_commands_stay_inside_the_supervisor_job() {
    if !live_enabled() {
        eprintln!("AAS_LIVE_TESTS not set; skipping");
        return;
    }
    let codex = codex();
    for mode in aas_adapter_codex::testing::sandbox_modes() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        let supervisor = Supervisor::new(
            &dir.path().join("state"),
            SupervisorPolicy {
                prevent_sleep: false,
                ..Default::default()
            },
        )
        .unwrap();
        let mut cleanup = Cleanup::new();

        // A command that exits: its exit code comes through the sandbox.
        let mut exits = supervisor
            .spawn(
                SpawnSpec::new(format!("codex-sandbox-{mode}-exit"), &codex, &workspace)
                    .args(sandbox_args(mode).iter().take_while(|a| *a != "--"))
                    .args(["--", "cmd.exe", "/d", "/c", "exit 7"]),
            )
            .await
            .unwrap_or_else(|e| panic!("{mode}: {e}"));
        drop(exits.stdin.take());
        let info = tokio::time::timeout(PROBE_TIMEOUT, exits.handle.wait())
            .await
            .unwrap_or_else(|_| panic!("{mode}: the sandboxed command did not finish"));
        assert_eq!(info.code, Some(7), "{mode}: {info:?}");
        assert_eq!(info.stopped, None, "{mode}: {info:?}");

        // A command that runs until the tree is terminated.
        let mut child = supervisor
            .spawn(
                SpawnSpec::new(format!("codex-sandbox-{mode}"), &codex, &workspace)
                    .args(sandbox_args(mode)),
            )
            .await
            .unwrap_or_else(|e| panic!("{mode}: {e}"));
        let mut lines = AsyncBufReader::new(child.stdout.take().expect("stdout")).lines();
        let line = tokio::time::timeout(PROBE_TIMEOUT, async {
            loop {
                match lines.next_line().await.expect("reading stdout") {
                    Some(line) if line.trim_start().starts_with('{') => return line,
                    Some(_) => continue,
                    None => panic!(
                        "{mode}: Codex ended without the probe's report; stderr: {}",
                        child.handle.stderr_tail()
                    ),
                }
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "{mode}: no probe report within {PROBE_TIMEOUT:?}; stderr: {}",
                child.handle.stderr_tail()
            )
        });
        let probe = Probe::parse(&line);
        for p in &probe.procs {
            cleanup.process(*p);
        }
        probe.check(mode);
        assert!(
            probe.procs.iter().all(Proc::alive),
            "{mode}: the probe and its children run: {probe:?}"
        );
        // Codex's sandbox puts its processes in a job of its own; ours holds them too, also a
        // process whose start with CREATE_BREAKAWAY_FROM_JOB succeeded (Codex's job allows it).
        for p in &probe.procs {
            assert!(
                child.handle.tree_contains(p.pid, p.created).unwrap(),
                "{mode}: {p:?} is in the supervised tree ({probe:?})"
            );
        }

        child.handle.kill(StopReason::User);
        let info = tokio::time::timeout(DEATH, child.handle.wait())
            .await
            .unwrap_or_else(|_| panic!("{mode}: the tree did not end"));
        assert_eq!(info.stopped, Some(StopReason::User), "{mode}: {info:?}");
        let survivors = wait_dead(&probe.procs).await;
        assert!(
            survivors.is_empty(),
            "{mode}: sandboxed processes outlived the terminated tree: {survivors:?} ({probe:?})"
        );
    }
}

#[tokio::test]
#[ignore = "runs the Codex CLI's sandbox (no model tokens); set AAS_LIVE_TESTS=1 and pass --ignored"]
async fn codex_sandboxed_commands_die_with_the_supervising_process() {
    if !live_enabled() {
        eprintln!("AAS_LIVE_TESTS not set; skipping");
        return;
    }
    let codex = codex();
    for mode in aas_adapter_codex::testing::sandbox_modes() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        let mut cleanup = Cleanup::new();
        let mut host = Command::new(proc::bin_path("aas-supervisor-host"))
            .arg("exec")
            .arg(dir.path().join("state"))
            .arg(&workspace)
            .arg("--")
            .arg(&codex)
            .args(sandbox_args(mode))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("start aas-supervisor-host");
        let host_proc = Proc::of(host.id()).expect("the host runs");
        cleanup.process(host_proc);
        let stdout = host.stdout.take().expect("host stdout");
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let deadline = Instant::now() + PROBE_TIMEOUT;
        let line = loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match rx.recv_timeout(left) {
                Ok(line) if line.trim_start().starts_with('{') => break line,
                Ok(_) => continue,
                Err(e) => panic!("{mode}: no probe report through the host: {e}"),
            }
        };
        let probe = Probe::parse(&line);
        for p in &probe.procs {
            cleanup.process(*p);
        }
        probe.check(mode);

        // TerminateProcess on the "daemon": the OS closes its job handles, and
        // KILL_ON_JOB_CLOSE ends everything in the job, nested sandbox jobs included.
        host.kill().expect("kill the host");
        host.wait().expect("reap the host");
        let survivors = wait_dead(&probe.procs).await;
        assert!(
            survivors.is_empty(),
            "{mode}: sandboxed processes outlived the killed daemon: {survivors:?} ({probe:?})"
        );
    }
}
