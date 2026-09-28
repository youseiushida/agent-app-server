//! The whole chain with real binaries: `agent-app-server-daemon` (watchdog) →
//! `agent-app-server run --background` → fake agent processes (`agent-app-server fake agent`,
//! the fake harness in process mode). Each level runs in the Job Object of the level above
//! (nested jobs).

#![cfg(windows)]

mod common;

use std::time::Duration;

use common::*;

/// Short watchdog timings so that restarts and liveness checks happen within a test.
const FAST_WATCHDOG: &str = r#"
watchdog_restart_delay_min = "200ms"
watchdog_restart_delay_max = "1s"
liveness_interval = "500ms"
liveness_deadline = "1s"
liveness_timeout = "2s"
end_session_deadline = "4s"
"#;

#[tokio::test(flavor = "multi_thread")]
async fn the_watchdog_restarts_a_killed_daemon_and_takes_everything_down_with_it() {
    let dirs = Dirs::new();
    dirs.write_config(&dirs.admin.to_string(), FAST_WATCHDOG);
    let mut watchdog = start_watchdog(&dirs);
    dirs.wait_for_daemon().await;
    let daemon = eventually("the daemon on the watchdog's ledger", || {
        dirs.ledger("watchdog")
            .into_iter()
            .find(|e| e.label == "daemon" && alive(e))
    })
    .await;

    let (_, token) = dirs.pair("phone").await;
    let mut ws = Ws::connect(dirs.listen, &token).await;
    ws.start_long_turn(&dirs.root, "a").await;
    let agent = agent_process(&dirs, &[]).await;

    // A crash of the daemon (TerminateProcess): its agents go with its jobs, and the watchdog
    // starts a new daemon.
    assert!(aas_supervisor::terminate_if_same(daemon.pid, daemon.created).unwrap());
    wait_all_gone(&[daemon.clone(), agent.clone()], Duration::from_secs(15));
    let restarted = eventually("a restarted daemon", || {
        dirs.ledger("watchdog")
            .into_iter()
            .find(|e| e.label == "daemon" && e.pid != daemon.pid && alive(e))
    })
    .await;
    let status = dirs.wait_for_daemon().await;
    assert_eq!(status.running_turns, 0, "the new daemon starts clean");
    assert!(
        dirs.watchdog_log().contains("restarting the daemon"),
        "{}",
        dirs.watchdog_log()
    );
    let mut ws = Ws::connect(dirs.listen, &token).await;
    ws.start_long_turn(&dirs.root, "b").await;
    let second_agent = agent_process(&dirs, &[agent.pid]).await;

    // Killing the watchdog takes the daemon and every agent with it (nested Job Objects).
    watchdog.child.kill().unwrap();
    watchdog.child.wait().unwrap();
    wait_all_gone(&[restarted, second_agent], Duration::from_secs(15));
    // The new daemon recorded the turn the crash ended; the second one ended with the tree.
    assert_eq!(
        turn_error_kinds(&dirs).first(),
        Some(&Some("daemonRestarted".to_owned()))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_configuration_error_stops_the_watchdog_with_2() {
    let dirs = Dirs::new();
    // The admin listener must be loopback: the daemon refuses to start (exit code 2).
    dirs.write_config("0.0.0.0:7879", FAST_WATCHDOG);
    let mut watchdog = start_watchdog(&dirs);
    let status = watchdog.wait(PATIENCE);
    assert_eq!(status.code(), Some(2));
    let log = dirs.watchdog_log();
    assert!(log.contains("configuration"), "{log}");
    assert_eq!(
        log.matches("daemon started").count(),
        1,
        "not retried: {log}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_second_watchdog_for_the_same_data_folder_exits_with_3() {
    let dirs = Dirs::new();
    dirs.write_config(&dirs.admin.to_string(), FAST_WATCHDOG);
    let _first = start_watchdog(&dirs);
    dirs.wait_for_daemon().await;
    let mut second = start_watchdog(&dirs);
    assert_eq!(second.wait(PATIENCE).code(), Some(3));
    // The first one and its daemon are untouched.
    dirs.wait_for_daemon().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_requested_stop_ends_the_watchdog_with_0() {
    let dirs = Dirs::new();
    dirs.write_config(&dirs.admin.to_string(), FAST_WATCHDOG);
    let mut watchdog = start_watchdog(&dirs);
    dirs.wait_for_daemon().await;
    let (code, _, err) = dirs.cli(&["stop"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(watchdog.wait(PATIENCE).code(), Some(0));
    assert!(dirs.watchdog_log().contains("stopped on request"));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_end_of_the_session_reaches_the_daemon_through_the_watchdog_and_nothing_restarts() {
    let dirs = Dirs::new();
    dirs.write_config(&dirs.admin.to_string(), FAST_WATCHDOG);
    let mut watchdog = start_watchdog(&dirs);
    dirs.wait_for_daemon().await;
    let daemon = eventually("the daemon", || {
        dirs.ledger("watchdog")
            .into_iter()
            .find(|e| e.label == "daemon" && alive(e))
    })
    .await;
    let (_, token) = dirs.pair("phone").await;
    let mut ws = Ws::connect(dirs.listen, &token).await;
    ws.start_long_turn(&dirs.root, "a").await;
    let agent = agent_process(&dirs, &[]).await;

    // Windows ends the session: the watchdog's window gets the messages and blocks until the
    // daemon (told through its control line) has stopped.
    let window = eventually("the watchdog's end-session window", || {
        end_session_windows(watchdog.pid()).first().copied()
    })
    .await;
    let started = std::time::Instant::now();
    tokio::task::spawn_blocking(move || end_the_session(window))
        .await
        .unwrap();
    assert!(started.elapsed() < Duration::from_secs(6));
    assert_eq!(watchdog.wait(Duration::from_secs(10)).code(), Some(0));
    wait_all_gone(&[daemon, agent], Duration::from_secs(10));
    let log = dirs.watchdog_log();
    assert!(
        log.contains("ending the session") && !log.contains("restarting the daemon"),
        "{log}"
    );
    assert_eq!(
        turn_error_kinds(&dirs),
        vec![Some("systemShutdown".to_owned())]
    );
}
