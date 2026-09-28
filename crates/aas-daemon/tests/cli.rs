//! The management CLI against a real background daemon (`agent-app-server run --background`)
//! in temporary config and data folders.

mod common;

use std::time::Duration;

use common::*;

#[test]
fn init_writes_the_configuration_into_the_given_folders() {
    let dirs = Dirs::new();
    let (code, out, err) = dirs.cli(&["init"]);
    assert_eq!(code, 0, "{out}{err}");
    let file = dirs.config_dir.join("config.toml");
    assert!(file.is_file());
    assert!(out.contains(&file.display().to_string()), "{out}");
    assert!(out.contains("admin 127.0.0.1:7879"), "{out}");
    let (code, out, _) = dirs.cli(&["init"]);
    assert_eq!(code, 0);
    assert!(out.contains("already exists"), "{out}");
    // The environment variables select the folders too.
    let other = tempfile::tempdir().unwrap();
    let status = std::process::Command::new(CLI)
        .arg("init")
        .env("AAS_CONFIG_DIR", other.path().join("cfg"))
        .env("AAS_DATA_DIR", other.path().join("data"))
        .stdout(std::process::Stdio::null())
        .status()
        .unwrap();
    assert!(status.success());
    assert!(other.path().join("cfg").join("config.toml").is_file());
}

#[test]
fn an_invalid_configuration_exits_with_2() {
    let dirs = Dirs::new();
    std::fs::create_dir_all(&dirs.config_dir).unwrap();
    std::fs::write(dirs.config_dir.join("config.toml"), "[server]\nbogus = 1\n").unwrap();
    let (code, _, err) = dirs.cli(&["run", "--background"]);
    assert_eq!(code, 2, "{err}");
    // A non-loopback admin listener is refused the same way.
    dirs.write_config("0.0.0.0:7879", "");
    let (code, _, err) = dirs.cli(&["run", "--background"]);
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("admin_listen"), "{err}");
    // So are policy values below their lower bound (a zero heartbeat used to panic every
    // connection, a zero maintenance interval to spin), each one named.
    dirs.write_config(
        &dirs.admin.to_string(),
        "heartbeat_interval = \"0s\"\nmaintenance_interval = \"0s\"\nliveness_interval = \"0s\"",
    );
    let (code, _, err) = dirs.cli(&["run", "--background"]);
    assert_eq!(code, 2, "{err}");
    for key in [
        "heartbeat_interval",
        "maintenance_interval",
        "liveness_interval",
    ] {
        assert!(
            err.contains(&format!("policy.{key} must be at least")),
            "{err}"
        );
    }
    // So is a git.command that does not exist.
    dirs.write_config(&dirs.admin.to_string(), "");
    let mut text = std::fs::read_to_string(dirs.config_dir.join("config.toml")).unwrap();
    text.push_str("\n[git]\ncommand = \"definitely-not-a-git-binary-aas\"\n");
    std::fs::write(dirs.config_dir.join("config.toml"), text).unwrap();
    let (code, _, err) = dirs.cli(&["run", "--background"]);
    assert_eq!(code, 2, "{err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_cli_manages_a_background_daemon() {
    let dirs = Dirs::new();
    dirs.write_config(&dirs.admin.to_string(), "");
    let (mut daemon, ready) = start_background_daemon(&dirs);
    assert_eq!(ready["event"], "ready");
    assert_eq!(ready["listen"], dirs.listen.to_string());
    assert_eq!(ready["adminListen"], dirs.admin.to_string());

    let (code, out, err) = dirs.cli(&["status"]);
    assert_eq!(code, 0, "{err}");
    assert!(
        out.contains(&format!("listening on:      {}", dirs.listen)),
        "{out}"
    );
    assert!(
        out.contains(&format!("admin listener:    {}", dirs.admin)),
        "{out}"
    );
    assert!(out.contains("draining:          false"), "{out}");

    // A second daemon on the same data folder is refused as busy (the watchdog retries).
    let (code, _, err) = dirs.cli(&["run", "--background"]);
    assert_eq!(code, 4, "{err}");

    // pair → the phone's side of pairing → devices → revoke.
    let (code, out, err) = dirs.cli(&["pair"]);
    assert_eq!(code, 0, "{err}");
    let code_line = out
        .lines()
        .find(|l| l.contains("enter the code manually:"))
        .unwrap_or_else(|| panic!("{out}"));
    let pairing_code = code_line.rsplit(' ').next().unwrap();
    let (device_id, _token) = pair_with_code(dirs.listen, pairing_code, "phone").await;
    let (code, out, _) = dirs.cli(&["devices"]);
    assert_eq!(code, 0);
    assert!(out.contains(&device_id) && out.contains("phone"), "{out}");
    let (code, out, err) = dirs.cli(&["revoke", &device_id]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("revoked"));
    let (code, out, _) = dirs.cli(&["devices"]);
    assert_eq!(code, 0);
    assert!(out.contains("no paired devices"), "{out}");
    let (code, _, err) = dirs.cli(&["revoke", &device_id]);
    assert_eq!(code, 1);
    assert!(err.contains("404"), "{err}");

    // doctor passes although no agent CLI is installed or configured.
    let (code, out, err) = dirs.cli(&["doctor"]);
    assert_eq!(code, 0, "doctor failed:\n{out}{err}");
    assert!(out.contains("[OK  ] daemon"), "{out}");
    assert!(out.contains("[OK  ] harness fake"), "{out}");
    assert!(out.contains("] sleep"), "{out}");
    assert!(out.contains("] keep-awake"), "{out}");
    assert!(out.contains("] autostart"), "{out}");

    // The harnesses are probed again on request (e.g. after logging in to a CLI).
    let (code, out, err) = dirs.cli(&["harness", "refresh"]);
    assert_eq!(code, 0, "{err}");
    assert!(
        out.lines()
            .any(|l| l.starts_with("fake") && l.contains(" available")),
        "{out}"
    );
    let (code, out, err) = dirs.cli(&["harness", "refresh", "fake"]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("fake"), "{out}");
    let (code, _, err) = dirs.cli(&["harness", "refresh", "nope"]);
    assert_eq!(code, 1);
    assert!(err.contains("404"), "{err}");

    let (code, out, err) = dirs.cli(&["stop"]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("stopping"));
    assert_eq!(daemon.wait(PATIENCE).code(), Some(0));
    let (code, _, err) = dirs.cli(&["status"]);
    assert_eq!(code, 1);
    assert!(err.contains("not running"), "{err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn stop_drain_waits_for_running_turns_and_a_later_stop_ends_it() {
    let dirs = Dirs::new();
    dirs.write_config(&dirs.admin.to_string(), "");
    let (mut daemon, _) = start_background_daemon(&dirs);
    let (_, token) = dirs.pair("phone").await;
    let mut ws = Ws::connect(dirs.listen, &token).await;
    ws.start_long_turn(&dirs.root, "a").await;
    let agent = agent_process(&dirs, &[]).await;

    let (code, out, err) = dirs.cli(&["stop", "--drain"]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("after running turns finish"));
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        daemon.child.try_wait().unwrap().is_none(),
        "the drain waits for the turn"
    );
    let (code, out, _) = dirs.cli(&["status"]);
    assert_eq!(code, 0);
    assert!(
        out.contains("draining:          true") && out.contains("running turns:     1"),
        "{out}"
    );

    let (code, _, err) = dirs.cli(&["stop"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(daemon.wait(PATIENCE).code(), Some(0));
    wait_all_gone(&[agent], Duration::from_secs(10));
    assert_eq!(
        turn_error_kinds(&dirs),
        vec![Some("daemonShutdown".to_owned())]
    );
}

#[cfg(windows)]
#[tokio::test(flavor = "multi_thread")]
async fn the_end_of_the_session_stops_a_foreground_daemon_with_its_own_reason() {
    let dirs = Dirs::new();
    dirs.write_config(&dirs.admin.to_string(), "");
    let mut daemon = Background {
        child: dirs
            .command(CLI)
            .arg("run")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap(),
        name: "agent-app-server run",
    };
    dirs.wait_for_daemon().await;
    let (_, token) = dirs.pair("phone").await;
    let mut ws = Ws::connect(dirs.listen, &token).await;
    ws.start_long_turn(&dirs.root, "a").await;
    let agent = agent_process(&dirs, &[]).await;
    let window = eventually("the end-session window", || {
        end_session_windows(daemon.pid()).first().copied()
    })
    .await;
    let started = std::time::Instant::now();
    tokio::task::spawn_blocking(move || end_the_session(window))
        .await
        .unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(6),
        "WM_ENDSESSION returned within the deadline"
    );
    assert_eq!(daemon.wait(Duration::from_secs(10)).code(), Some(0));
    wait_all_gone(&[agent], Duration::from_secs(10));
    assert_eq!(
        turn_error_kinds(&dirs),
        vec![Some("systemShutdown".to_owned())]
    );
}
