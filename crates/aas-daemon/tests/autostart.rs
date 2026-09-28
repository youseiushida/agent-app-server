//! Autostart against the real Task Scheduler (design.md §18.3). Opt-in, because it registers
//! tasks of the current user and takes about three minutes:
//!
//! ```text
//! AAS_SYSTEM_TESTS=1 cargo test -p aas-daemon --test autostart -- --ignored --nocapture
//! ```
//!
//! It registers temporary tasks (unique names) through the code path of `autostart install`
//! (`autostart::definitions`, `task_xml`, `register`), with a stand-in for the watchdog: a copy
//! of `agent-app-server-daemon.exe` alone in a folder, which starts, logs and exits with code
//! 2 because `agent-app-server.exe` is not next to it. It measures
//!
//! * that a registered task is found and a missing one is reported as missing (the explicit
//!   `HRESULT_FROM_WIN32(ERROR_FILE_NOT_FOUND)` of the COM API),
//! * that the task runs and Task Scheduler records the exit code,
//! * whether Task Scheduler's restart-on-failure restarts a program that exits with a
//!   non-zero code (it does not: that is why autostart has a keep-alive task),
//! * that the keep-alive task starts the watchdog again, and that such a start honours the
//!   record of a deliberate end,
//! * what restart-on-failure does for a program that cannot be started (reported only).
//!
//! The tasks are always deleted, also when an assertion fails or the test panics.

#![cfg(windows)]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use aas_daemon::autostart::{self, TaskAction, TaskDefinition, TaskStatus, TaskTrigger};
use aas_daemon::config::Paths;

const WATCHDOG: &str = env!("CARGO_BIN_EXE_agent-app-server-daemon");

/// Task Scheduler's shortest restart and repetition interval.
const MINUTE: Duration = Duration::from_secs(60);
/// How long runs are counted: two and a half restart intervals.
const OBSERVE: Duration = Duration::from_secs(150);
/// How long the first run may take to be recorded.
const FIRST_RUN: Duration = Duration::from_secs(30);

/// Deletes the tasks it holds when dropped (also while unwinding from a panic).
struct Tasks {
    rt: tokio::runtime::Runtime,
    names: Vec<String>,
}

impl Tasks {
    fn register(&mut self, def: &TaskDefinition) {
        // Held before registering: a registration that fails half-way is deleted too.
        self.names.push(def.name.clone());
        self.rt
            .block_on(autostart::register(def))
            .unwrap_or_else(|e| panic!("registering {}: {e:#}", def.name));
    }

    fn query(&self, name: &str) -> Option<TaskStatus> {
        self.rt
            .block_on(autostart::query(name))
            .unwrap_or_else(|e| panic!("querying {name}: {e:#}"))
    }
}

impl Drop for Tasks {
    fn drop(&mut self) {
        for name in self.names.drain(..) {
            match self.rt.block_on(autostart::delete(&name)) {
                Ok(_) => eprintln!("deleted the task {name}"),
                Err(e) => eprintln!(
                    "COULD NOT DELETE the task {name}: {e:#} (remove it with `schtasks /Delete /TN {name} /F`)"
                ),
            }
        }
    }
}

/// Distinct start times of a task seen while polling.
#[derive(Default)]
struct Runs(Vec<f64>);

impl Runs {
    fn note(&mut self, status: &Option<TaskStatus>) {
        if let Some(run) = status.as_ref().and_then(|s| s.last_run)
            && !self.0.contains(&run)
        {
            self.0.push(run);
        }
    }
}

fn stand_in(dir: &Path) -> PathBuf {
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let exe = bin.join("agent-app-server-daemon.exe");
    std::fs::copy(WATCHDOG, &exe).unwrap();
    exe
}

#[test]
#[ignore = "registers Task Scheduler tasks; run with AAS_SYSTEM_TESTS=1 and --ignored"]
fn task_scheduler_runs_the_tasks_restarts_nothing_by_itself_and_the_keepalive_does() {
    if std::env::var_os("AAS_SYSTEM_TESTS").is_none_or(|v| v != "1") {
        eprintln!("set AAS_SYSTEM_TESTS=1 to run this system test");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths {
        config_dir: dir.path().join("cfg"),
        data_dir: dir.path().join("data"),
    };
    std::fs::create_dir_all(&paths.data_dir).unwrap();
    let exe = stand_in(dir.path());
    let user = autostart::current_user().unwrap();
    let id = format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis()
    );
    let mut tasks = Tasks {
        rt: tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap(),
        names: Vec::new(),
    };

    // The definitions `autostart install` registers, under temporary names; the logon task
    // with Task Scheduler's restart-on-failure switched on to measure it.
    let [mut logon, mut keepalive] = autostart::definitions(&exe, &paths, MINUTE, &user);
    logon.name = format!("aas-test-{id}");
    logon.restart_on_failure = Some((MINUTE, 3));
    keepalive.name = format!("aas-test-{id}-keepalive");
    // A program that cannot be started at all.
    let broken = TaskDefinition {
        name: format!("aas-test-{id}-missing-program"),
        description: "agent-app-server system test".into(),
        trigger: TaskTrigger::Logon,
        action: TaskAction {
            program: dir.path().join("no-such-program.exe"),
            args: Vec::new(),
            working_dir: paths.data_dir.clone(),
        },
        user: user.clone(),
        restart_on_failure: Some((MINUTE, 3)),
    };
    assert!(
        tasks
            .query(&format!("aas-test-{id}-never-registered"))
            .is_none(),
        "a missing task is reported as missing, not as an error"
    );
    tasks.register(&logon);
    tasks.register(&broken);
    for def in [&logon, &broken] {
        let status = tasks.query(&def.name).expect("registered");
        assert!(status.enabled, "{status:?}");
        assert_eq!(status.last_run, None, "not run yet: {status:?}");
    }

    // It runs: Task Scheduler records the stand-in's exit code (2, a configuration error).
    tasks.rt.block_on(autostart::run(&logon.name)).unwrap();
    tasks.rt.block_on(autostart::run(&broken.name)).unwrap();
    let asked = Instant::now();
    let first = loop {
        let status = tasks.query(&logon.name).unwrap();
        if status.last_run.is_some() && status.state != "running" && status.last_result == 2 {
            break status;
        }
        assert!(
            asked.elapsed() < FIRST_RUN,
            "the task did not run: {}",
            status.describe()
        );
        std::thread::sleep(Duration::from_millis(500));
    };
    eprintln!("first run: {}", first.describe());
    let log = paths.logs_dir().join("watchdog.log");
    let text = std::fs::read_to_string(&log).unwrap();
    assert!(text.contains("not found next to the watchdog"), "{text}");
    assert!(
        paths.watchdog_stopped_file().exists(),
        "a configuration error is a deliberate end"
    );

    // The keep-alive task from now on, then count the starts of each task.
    tasks.register(&keepalive);
    let (mut logon_runs, mut keepalive_runs, mut broken_runs) =
        (Runs::default(), Runs::default(), Runs::default());
    let observing = Instant::now();
    while observing.elapsed() < OBSERVE {
        logon_runs.note(&tasks.query(&logon.name));
        keepalive_runs.note(&tasks.query(&keepalive.name));
        broken_runs.note(&tasks.query(&broken.name));
        std::thread::sleep(Duration::from_secs(1));
    }
    let last = |name: &str| tasks.query(name).map(|s| s.describe()).unwrap_or_default();
    eprintln!("after {OBSERVE:?}:");
    eprintln!(
        "  exit code 2 with restart-on-failure every {MINUTE:?}: {} start(s) — {}",
        logon_runs.0.len(),
        last(&logon.name)
    );
    eprintln!(
        "  keep-alive every {MINUTE:?}: {} start(s) — {}",
        keepalive_runs.0.len(),
        last(&keepalive.name)
    );
    eprintln!(
        "  program that cannot be started, restart-on-failure every {MINUTE:?}: {} start(s) — {}",
        broken_runs.0.len(),
        last(&broken.name)
    );
    assert_eq!(
        logon_runs.0.len(),
        1,
        "restart-on-failure restarted a program that exited with a non-zero code: design.md §18.3 needs revisiting"
    );
    assert!(
        keepalive_runs.0.len() >= 2,
        "the keep-alive task repeats: {:?}",
        keepalive_runs.0
    );
    // Every keep-alive start honoured the record of the deliberate end: it ended with 0
    // without trying the daemon (which would have failed again, logged and exited with 2).
    let keepalive_status = tasks.query(&keepalive.name).unwrap();
    assert_eq!(
        keepalive_status.last_result,
        0,
        "{}",
        keepalive_status.describe()
    );
    let text = std::fs::read_to_string(&log).unwrap();
    assert_eq!(
        text.matches("not found next to the watchdog").count(),
        1,
        "the daemon was tried only by the explicit start: {text}"
    );

    // Deleted by the guard; a deleted task is missing, and deleting it again is no error.
    let names = tasks.names.clone();
    drop(tasks);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    for name in names {
        assert!(rt.block_on(autostart::query(&name)).unwrap().is_none());
        assert!(!rt.block_on(autostart::delete(&name)).unwrap());
    }
}
