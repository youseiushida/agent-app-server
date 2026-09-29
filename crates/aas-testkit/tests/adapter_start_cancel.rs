//! Adapter starts that are dropped half-way through the handshake must not leak the agent
//! process (the engine drops a start future when a thread is stopped while `starting`; a
//! transport drops probes when a request is cancelled).
//!
//! The "agent" is `aas-dummy-agent tree --depth 0`: it records itself, never answers its
//! handshake and ignores EOF on stdin, so only the adapter's staged stop can end it. Every
//! adapter must use the staged stop of design §4.3 (close stdin, wait `stop_grace`, then
//! terminate the tree through `aas_harness::StartGuard`), not an immediate kill: the process
//! must still be running until the grace period has passed.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use aas_harness::{
    AdapterContext, AdapterPolicy, HarnessAdapter, HarnessConfig, HarnessKind, StartMode,
    StartRequest,
};
use aas_protocol::{ThreadId, ThreadSettings};
use aas_supervisor::{Supervisor, SupervisorPolicy};
use aas_testkit::proc::{self, Cleanup, Proc};

/// Upper bound for the dummy agent to start and record itself.
const START: Duration = Duration::from_secs(60);
/// Upper bound for the abandoned process to be stopped (stop grace + termination).
const DEATH: Duration = Duration::from_secs(10);
/// Stop grace given to the adapters: long enough to tell a staged stop from an immediate kill.
const STOP_GRACE: Duration = Duration::from_millis(1500);

struct Setup {
    _dir: tempfile::TempDir,
    cwd: PathBuf,
    pids: PathBuf,
    config: HarnessConfig,
    ctx: AdapterContext,
    _cleanup: Cleanup,
}

fn setup(kind: HarnessKind, id: &str) -> Setup {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let pids = dir.path().join("pids");
    let mut cleanup = Cleanup::new();
    cleanup.dir(&pids);
    let supervisor = Supervisor::new(
        &dir.path().join("supervisor"),
        SupervisorPolicy {
            prevent_sleep: false,
            ..Default::default()
        },
    )
    .unwrap();
    let config = HarnessConfig {
        id: id.to_owned(),
        kind,
        display_name: None,
        command: proc::bin_path("aas-dummy-agent").display().to_string(),
        args: ["tree", "--depth", "0", "--width", "0", "--pid-dir"]
            .into_iter()
            .map(str::to_owned)
            .chain([pids.display().to_string()])
            .collect(),
        env: Default::default(),
        options: serde_json::Value::Null,
    };
    let ctx = AdapterContext {
        supervisor,
        state_dir: dir.path().join("state"),
        policy: AdapterPolicy {
            stop_grace: STOP_GRACE,
            max_line_bytes: 1 << 20,
            handshake_timeout: Duration::from_secs(120),
            ..AdapterPolicy::default()
        },
    };
    Setup {
        _dir: dir,
        cwd,
        pids,
        config,
        ctx,
        _cleanup: cleanup,
    }
}

async fn wait_recorded(dir: &Path) -> Vec<Proc> {
    let dir = dir.to_owned();
    tokio::task::spawn_blocking(move || proc::wait_for_pids(&dir, 1, START))
        .await
        .unwrap()
}

async fn wait_dead(procs: Vec<Proc>) -> Vec<Proc> {
    tokio::task::spawn_blocking(move || proc::wait_all_dead(&procs, DEATH))
        .await
        .unwrap()
}

/// Starts a session, drops the start future once the process runs, and checks that the
/// process is stopped.
async fn dropped_start_stops_the_process(adapter: Arc<dyn HarnessAdapter>, s: &Setup) {
    let req = StartRequest {
        thread_id: ThreadId::from("thr_cancelled"),
        cwd: s.cwd.clone(),
        settings: ThreadSettings::default(),
        mode: StartMode::New,
    };
    dropped_future_stops_the_process(adapter.start(req), s).await;
}

/// Polls `future` until the dummy agent it spawns has recorded itself, drops it (the caller
/// gives up during the handshake), and checks that the process is stopped by the staged stop:
/// still running during the grace period, gone afterwards.
async fn dropped_future_stops_the_process<F: std::future::Future>(future: F, s: &Setup) {
    let mut future = Box::pin(future);
    let procs = tokio::select! {
        _ = &mut future => panic!("the handshake cannot complete, yet the call returned"),
        procs = wait_recorded(&s.pids) => procs,
    };
    drop(future);
    let dropped = Instant::now();
    let dead = {
        let procs = procs.clone();
        tokio::task::spawn_blocking(move || {
            let deadline = Instant::now() + DEATH;
            while procs.iter().any(Proc::alive) {
                assert!(
                    Instant::now() < deadline,
                    "the process of an abandoned start kept running"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            Instant::now()
        })
        .await
        .unwrap()
    };
    assert!(
        dead.duration_since(dropped) >= STOP_GRACE,
        "the process was killed {:?} after the drop, before the stop grace ({STOP_GRACE:?}): not a staged stop",
        dead.duration_since(dropped)
    );
    assert_eq!(wait_dead(procs).await, Vec::<Proc>::new());
}

#[tokio::test(flavor = "multi_thread")]
async fn claude_start_dropped_during_initialize_stops_the_process() {
    let s = setup(HarnessKind::Claude, "claude");
    let adapter = Arc::new(aas_adapter_claude::ClaudeAdapter::new(
        s.config.clone(),
        s.ctx.clone(),
    ));
    dropped_start_stops_the_process(adapter, &s).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn pi_start_dropped_during_the_handshake_stops_the_process() {
    let s = setup(HarnessKind::Pi, "pi");
    let adapter = Arc::new(aas_adapter_pi::PiAdapter::new(
        s.config.clone(),
        s.ctx.clone(),
    ));
    dropped_start_stops_the_process(adapter, &s).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn acp_start_dropped_during_initialize_stops_the_process() {
    let s = setup(HarnessKind::Acp, "devin");
    let adapter = Arc::new(aas_adapter_acp::AcpAdapter::new(
        s.config.clone(),
        s.ctx.clone(),
    ));
    dropped_start_stops_the_process(adapter, &s).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn codex_start_dropped_during_initialize_stops_the_process() {
    let s = setup(HarnessKind::Codex, "codex");
    let adapter = Arc::new(aas_adapter_codex::CodexAdapter::new(
        s.config.clone(),
        s.ctx.clone(),
    ));
    dropped_start_stops_the_process(adapter, &s).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn codex_listing_dropped_during_initialize_stops_its_short_lived_server() {
    // Without a live session, `commands` starts a short-lived app-server for the request.
    let s = setup(HarnessKind::Codex, "codex");
    let adapter = aas_adapter_codex::CodexAdapter::new(s.config.clone(), s.ctx.clone());
    let ctx = aas_harness::CommandContext {
        cwd: s.cwd.clone(),
        native_session_id: None,
        project_trusted: None,
    };
    dropped_future_stops_the_process(adapter.commands(ctx), &s).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn acp_listing_dropped_during_initialize_stops_its_short_lived_process() {
    let s = setup(HarnessKind::Acp, "devin");
    let adapter = aas_adapter_acp::AcpAdapter::new(s.config.clone(), s.ctx.clone());
    dropped_future_stops_the_process(adapter.list_native_sessions(&s.cwd), &s).await;
}
