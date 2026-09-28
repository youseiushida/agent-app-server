//! Live test of the engine with the installed `codex` CLI: background terminals keep the thread's
//! process past the idle wait (D1), are shown and stopped through the protocol, and the idle stop
//! follows once Codex lists none.
//!
//! The model is the scripted one of `mock_model` in a temporary `CODEX_HOME` (no tokens, the
//! user's Codex configuration and sessions untouched). Run with:
//! `AAS_LIVE_TESTS=1 cargo test -p aas-adapter-codex --test live_engine -- --ignored`

mod mock_model;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use aas_adapter_codex::CodexAdapter;
use aas_core::{Engine, EngineConfig, HarnessRegistry, Policy, RequestCtx};
use aas_harness::{AdapterContext, AdapterPolicy, HarnessAdapter, HarnessConfig};
use aas_protocol::methods::{spec, *};
use aas_protocol::*;
use aas_supervisor::{Supervisor, SupervisorPolicy};
use mock_model::{LONG, MockModel, SHORT, background_script};
use serde_json::Value;

/// Upper bound of any single wait of the test (test harness only).
const WAIT: Duration = Duration::from_secs(120);
/// How often the test reads the thread while it waits (test harness only).
const POLL: Duration = Duration::from_millis(100);

fn crid() -> String {
    static N: AtomicU64 = AtomicU64::new(0);
    format!("crid-{}", N.fetch_add(1, Ordering::SeqCst))
}

struct Env {
    engine: Arc<Engine>,
    ctx: RequestCtx,
}

impl Env {
    async fn call<M: MethodSpec>(&self, params: M::Params) -> Result<M::Result, RpcError> {
        let req = ClientRequest::parse(M::NAME, Some(serde_json::to_value(params).unwrap()))?;
        let value = self.engine.handle(&self.ctx, req).await?;
        Ok(
            serde_json::from_value(value)
                .unwrap_or_else(|e| panic!("{}: bad result: {e}", M::NAME)),
        )
    }

    async fn read(&self, thread: &ThreadId) -> ThreadReadResult {
        self.call::<spec::ThreadRead>(ThreadReadParams {
            thread_id: thread.clone(),
            before_turn_index: None,
            limit_turns: Some(100),
        })
        .await
        .unwrap()
    }

    async fn wait_until(
        &self,
        thread: &ThreadId,
        what: &str,
        pred: impl Fn(&ThreadReadResult) -> bool,
    ) -> ThreadReadResult {
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            let read = self.read(thread).await;
            if pred(&read) {
                return read;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for {what}: {read:#?}"
            );
            tokio::time::sleep(POLL).await;
        }
    }
}

fn task<'a>(read: &'a ThreadReadResult, native: &str) -> Option<&'a BackgroundTask> {
    read.background_tasks.iter().find(|t| t.native_id == native)
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "runs the installed codex CLI; set AAS_LIVE_TESTS=1 and pass --ignored"]
async fn live_engine_keeps_codex_background_terminals_until_they_end() {
    if std::env::var_os("AAS_LIVE_TESTS").is_none() {
        eprintln!("AAS_LIVE_TESTS not set; skipping");
        return;
    }
    let model = MockModel::start(background_script).await;
    let dir = tempfile::tempdir().unwrap();
    let home = mock_model::codex_home(dir.path(), &model.base_url);
    let data = dir.path().join("data");
    let root = dir.path().join("projects");
    let project_dir = root.join("p");
    std::fs::create_dir_all(&project_dir).unwrap();
    let root = dunce::canonicalize(&root).unwrap();
    // A short idle wait: the process must outlive it many times while the terminals run.
    let idle = Duration::from_secs(1);
    let policy = Policy {
        idle_process_ttl: idle,
        background_progress_interval: Duration::ZERO,
        ..Policy::default()
    };
    let supervisor = Supervisor::new(
        &data.join("supervisor"),
        SupervisorPolicy {
            prevent_sleep: false,
            ..policy.supervisor_policy()
        },
    )
    .unwrap();
    let adapter: Arc<dyn HarnessAdapter> = Arc::new(CodexAdapter::new(
        HarnessConfig {
            id: "codex".into(),
            kind: HarnessKind::Codex,
            display_name: None,
            command: "codex".into(),
            args: Vec::new(),
            env: [("CODEX_HOME".to_owned(), home.to_string_lossy().into_owned())].into(),
            options: Value::Null,
        },
        AdapterContext {
            supervisor: supervisor.clone(),
            state_dir: data.join("adapters").join("codex"),
            policy: AdapterPolicy::default(),
        },
    ));
    let engine = Engine::start(
        EngineConfig {
            data_dir: data.clone(),
            server_name: "test".into(),
            hostname: "host".into(),
            project_roots: vec![root.clone()],
            policy,
            heuristics: Default::default(),
            git: None,
        },
        HarnessRegistry::new(vec![adapter]),
        supervisor.clone(),
    )
    .await
    .unwrap();
    let env = Env {
        engine,
        ctx: RequestCtx {
            device_id: DeviceId::from("dev_test"),
        },
    };
    let project = env
        .call::<spec::ProjectOpen>(ProjectOpenParams {
            client_request_id: crid(),
            path: root.join("p").display().to_string(),
            name: None,
        })
        .await
        .unwrap()
        .project;
    let thread = env
        .call::<spec::ThreadCreate>(ThreadCreateParams {
            client_request_id: crid(),
            project_id: project.id,
            harness_id: "codex".into(),
            settings: Some(ThreadSettings {
                permission_mode: Some("fullAccess".into()),
                ..Default::default()
            }),
            workspace: None,
            title: None,
            input: None,
        })
        .await
        .unwrap()
        .thread;
    let turn = env
        .call::<spec::TurnStart>(TurnStartParams {
            client_request_id: crid(),
            thread_id: thread.id.clone(),
            input: vec![InputPart::Text {
                text: "ROLE=TERM# Start two long commands.".into(),
            }],
            delivery: Delivery::Auto,
        })
        .await
        .unwrap()
        .turn_id
        .expect("a turn");

    // The turn ends with both commands running as background terminals.
    let read = env
        .wait_until(&thread.id, "the turn to complete", |r| {
            r.turns
                .iter()
                .any(|t| t.id == turn && t.status == TurnStatus::Completed)
        })
        .await;
    for (native, command) in [("mock_TERM_0", LONG), ("mock_TERM_1", SHORT)] {
        let t = task(&read, native).unwrap_or_else(|| panic!("no task {native}: {read:#?}"));
        assert_eq!(
            (t.kind, t.title.as_str(), t.status),
            (
                BackgroundTaskKind::Shell,
                command,
                BackgroundTaskStatus::Running
            )
        );
        assert!(t.stoppable);
        let origin = t.origin_item_id.clone().expect("launching item");
        let item = read
            .items
            .iter()
            .find(|i| i.id == origin)
            .unwrap_or_else(|| panic!("item {origin}: {read:#?}"));
        assert_eq!(item.status, ItemStatus::Backgrounded);
        assert_eq!(item.background_task_id.as_ref(), Some(&t.id));
    }
    let running = env.call::<spec::ThreadGet>(ThreadGetParams {
        thread_id: thread.id.clone(),
    });
    let t = running.await.unwrap().thread;
    assert_eq!((t.status, t.background.running), (ThreadStatus::Ready, 2));

    // Many idle waits later the process is still there: Codex lists the terminals as running.
    tokio::time::sleep(idle * 4).await;
    let t = env
        .call::<spec::ThreadGet>(ThreadGetParams {
            thread_id: thread.id.clone(),
        })
        .await
        .unwrap()
        .thread;
    assert_eq!(t.status, ThreadStatus::Ready, "not idle-stopped while busy");
    assert_eq!(supervisor.running_count(), 1);

    // The long one is stopped from the phone; the short one ends by itself.
    let long = task(&env.read(&thread.id).await, "mock_TERM_0")
        .unwrap()
        .clone();
    let stopped = env
        .call::<spec::BackgroundTaskStop>(BackgroundTaskStopParams {
            client_request_id: crid(),
            thread_id: thread.id.clone(),
            task_id: long.id.clone(),
        })
        .await
        .unwrap()
        .task;
    assert!(stopped.stop_requested_at.is_some());
    let read = env
        .wait_until(&thread.id, "both terminals to end", |r| {
            ["mock_TERM_0", "mock_TERM_1"]
                .iter()
                .all(|n| task(r, n).is_some_and(|t| t.status != BackgroundTaskStatus::Running))
        })
        .await;
    let long = task(&read, "mock_TERM_0").unwrap();
    assert_eq!(
        (long.status, long.end_reason),
        (
            BackgroundTaskStatus::Stopped,
            Some(BackgroundEndReason::Harness)
        )
    );
    assert_eq!(long.result.as_ref().and_then(|r| r.exit_code), Some(-1));
    let short = task(&read, "mock_TERM_1").unwrap();
    assert_eq!(short.status, BackgroundTaskStatus::Completed);
    let result = short.result.as_ref().expect("result");
    assert_eq!(result.exit_code, Some(0));
    assert!(
        result
            .output
            .as_deref()
            .is_some_and(|o| o.contains("SHORT_DONE"))
    );

    // Nothing keeps the process now: the idle stop follows.
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let t = env
            .call::<spec::ThreadGet>(ThreadGetParams {
                thread_id: thread.id.clone(),
            })
            .await
            .unwrap()
            .thread;
        if t.status == ThreadStatus::Idle && supervisor.running_count() == 0 {
            assert_eq!(t.background.running, 0);
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the idle stop did not come: {t:#?}"
        );
        tokio::time::sleep(POLL).await;
    }
    assert!(model.errors.lock().is_empty(), "{:?}", model.errors.lock());
    env.engine.shutdown(false).await;
    env.engine.close().await.unwrap();
}
