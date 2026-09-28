//! `agent-app-server doctor`: checks the installation and explains how to fix problems.

use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use aas_supervisor::{Supervisor, ToolSpec, resolve_program};
use serde_json::Value;

use crate::config::{Config, Paths};
use crate::tailscale::{self, ServeEntries, ServeTarget};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ok,
    Warn,
    Fail,
}

#[derive(Debug, Clone)]
pub struct Check {
    pub status: Status,
    pub name: String,
    pub detail: String,
}

fn check(status: Status, name: impl Into<String>, detail: impl Into<String>) -> Check {
    Check {
        status,
        name: name.into(),
        detail: detail.into(),
    }
}

async fn version_of(
    supervisor: &Supervisor,
    program: &Path,
    cwd: &Path,
    timeout: Duration,
) -> Result<String, String> {
    let spec = ToolSpec::new(program, cwd)
        .args(["--version"])
        .timeout(timeout);
    match supervisor.run_tool(spec).await {
        Ok(out) if out.success() => Ok(out
            .stdout_lossy()
            .lines()
            .next()
            .unwrap_or_default()
            .trim()
            .to_owned()),
        Ok(out) => Err(format!(
            "`--version` exited with {:?}: {}",
            out.code,
            out.stderr_lossy().trim()
        )),
        Err(e) => Err(e.to_string()),
    }
}

/// The checks about what `tailscale serve` publishes (from `tailscale serve status --json`).
/// An entry that cannot be read is a warning of its own; the others are all checked.
pub fn serve_checks(entries: &ServeEntries, listen: SocketAddr, admin: SocketAddr) -> Vec<Check> {
    let mut out = Vec::new();
    for entry in &entries.unreadable {
        out.push(check(
            Status::Warn,
            "tailscale serve",
            format!("cannot read the entry {entry}"),
        ));
    }
    let targets = &entries.targets;
    // Any path counts: the admin listener must not be reachable from the tailnet at all.
    let exposed: Vec<&ServeTarget> = targets.iter().filter(|t| t.reaches(admin)).collect();
    for t in &exposed {
        out.push(check(
            Status::Warn,
            "tailscale serve",
            format!(
                "{} forwards to the admin listener {admin}: the admin API must stay local. Remove it (`tailscale serve reset` and publish only {listen})",
                t.published
            ),
        ));
    }
    let daemon: Vec<&ServeTarget> = targets.iter().filter(|t| t.reaches(listen)).collect();
    match daemon.iter().find(|t| t.mount.as_deref() == Some("/") && t.forwards_to_root()) {
        Some(t) => out.push(check(Status::Ok, "tailscale serve", format!("{} publishes {listen}", t.published))),
        None => match daemon.first() {
            Some(t) => out.push(check(
                Status::Warn,
                "tailscale serve",
                format!("{} forwards to {listen}, but not as an HTTPS handler at /; run: tailscale serve --bg --https=443 http://{listen}", t.published),
            )),
            None => out.push(check(
                Status::Warn,
                "tailscale serve",
                format!("not publishing the daemon; run: tailscale serve --bg --https=443 http://{listen}"),
            )),
        },
    }
    out
}

pub async fn run(paths: &Paths) -> Vec<Check> {
    let mut out = Vec::new();
    let file = paths.config_file();
    let config = if !file.exists() {
        out.push(check(
            Status::Warn,
            "config",
            format!(
                "{} does not exist yet; `agent-app-server init` creates it",
                file.display()
            ),
        ));
        Config::initial()
    } else {
        match Config::load(&file) {
            Ok(c) => {
                out.push(check(Status::Ok, "config", file.display().to_string()));
                c
            }
            Err(e) => {
                out.push(check(Status::Fail, "config", format!("{e:#}")));
                return out;
            }
        }
    };
    let supervisor_policy = aas_supervisor::SupervisorPolicy {
        prevent_sleep: false,
        ..config.policy.supervisor_policy()
    };
    let supervisor = match Supervisor::new(&paths.data_dir.join("cli"), supervisor_policy) {
        Ok(s) => s,
        Err(e) => {
            out.push(check(
                Status::Fail,
                "data folder",
                format!("{}: {e}", paths.data_dir.display()),
            ));
            return out;
        }
    };
    out.push(check(
        Status::Ok,
        "data folder",
        paths.data_dir.display().to_string(),
    ));
    let version_timeout = config.daemon_policy.doctor_version_timeout;

    if config.projects.roots.is_empty() {
        out.push(check(
            Status::Warn,
            "project roots",
            "none configured: the app cannot browse or create projects ([projects] roots)",
        ));
    }
    for root in &config.projects.roots {
        let st = if root.is_dir() {
            Status::Ok
        } else {
            Status::Warn
        };
        out.push(check(st, "project root", root.display().to_string()));
    }

    let git = config.git.command.clone().unwrap_or_else(|| "git".into());
    match resolve_program(&git) {
        Ok(p) => match version_of(&supervisor, &p, &paths.data_dir, version_timeout).await {
            Ok(v) => out.push(check(Status::Ok, "git", format!("{v} ({})", p.display()))),
            Err(e) => out.push(check(Status::Warn, "git", e)),
        },
        Err(e) if config.git.command.is_some() => out.push(check(
            Status::Fail,
            "git",
            format!("git.command {git}: {e} (the daemon refuses to start with this setting)"),
        )),
        Err(e) => out.push(check(
            Status::Warn,
            "git",
            format!("{e}: diffs, worktrees and clone are disabled"),
        )),
    }

    for h in &config.harnesses {
        let name = format!("harness {}", h.id);
        if h.command.trim().is_empty() {
            out.push(check(
                Status::Ok,
                name,
                "runs inside the daemon (fake harness, inProcess mode)",
            ));
            continue;
        }
        match resolve_program(&h.command) {
            Ok(p) => match version_of(&supervisor, &p, &paths.data_dir, version_timeout).await {
                Ok(v) => out.push(check(Status::Ok, name, format!("{v} ({})", p.display()))),
                Err(e) => out.push(check(
                    Status::Fail,
                    name,
                    format!("{} does not run: {e}", p.display()),
                )),
            },
            Err(e) => out.push(check(Status::Fail, name, e.to_string())),
        }
    }
    for known in ["codex", "claude", "pi", "devin"] {
        let configured = config.harnesses.iter().any(|h| h.command == known);
        if !configured && resolve_program(known).is_ok() {
            out.push(check(
                Status::Warn,
                format!("harness {known}"),
                format!("`{known}` is installed but not configured in config.toml"),
            ));
        }
    }

    let db = paths.data_dir.join("aas.db");
    if db.exists() {
        match rusqlite::Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .and_then(|c| c.query_row("PRAGMA quick_check", [], |r| r.get::<_, String>(0)))
        {
            Ok(r) if r == "ok" => out.push(check(Status::Ok, "database", db.display().to_string())),
            Ok(r) => out.push(check(
                Status::Fail,
                "database",
                format!("integrity check: {r}"),
            )),
            Err(e) => out.push(check(Status::Fail, "database", e.to_string())),
        }
    }

    match crate::admin_client::AdminClient::from_config(paths, &config) {
        Ok(client) => match client.get::<aas_protocol::http::AdminStatusResponse>("/v1/admin/status").await {
            Ok(s) => out.push(check(
                Status::Ok,
                "daemon",
                format!(
                    "running {} on {} (admin {}; uptime {}s, {} agent processes, {} devices connected)",
                    s.version,
                    s.listen,
                    client.addr(),
                    s.uptime_ms / 1000,
                    s.running_processes,
                    s.connected_devices
                ),
            )),
            Err(e) => out.push(check(Status::Warn, "daemon", format!("{e:#}"))),
        },
        Err(e) => out.push(check(Status::Fail, "admin token", format!("{e:#}"))),
    }

    match crate::power_plan::read() {
        Ok(timeouts) => {
            let (status, detail) = crate::power_plan::evaluate(&timeouts, config.power.keep_awake);
            out.push(check(status, "sleep", detail));
        }
        Err(e) => out.push(check(
            Status::Warn,
            "sleep",
            format!("cannot read the power plan: {e}"),
        )),
    }
    match crate::power_plan::read_capabilities() {
        Ok(caps) => {
            let (status, detail) =
                crate::power_plan::explain_keep_awake(&caps, config.power.keep_awake);
            out.push(check(status, "keep-awake", detail));
        }
        Err(e) => out.push(check(
            Status::Warn,
            "keep-awake",
            format!("cannot read the sleep capabilities: {e}"),
        )),
    }

    let (listen, admin) = (config.server.listen, config.server.admin_listen);
    match tailscale::locate() {
        None => out.push(check(
            Status::Warn,
            "tailscale",
            "not found (neither on PATH nor where the Tailscale installer records it): install Tailscale on this PC and on the phone to connect from outside",
        )),
        Some(located) => {
            let ts = located.program;
            let status = supervisor.run_tool(ToolSpec::new(&ts, &paths.data_dir).args(["status", "--json"])).await;
            let parsed = status.ok().filter(|o| o.success()).and_then(|o| serde_json::from_slice::<Value>(&o.stdout).ok());
            match parsed {
                Some(v) if v["BackendState"] == "Running" => {
                    let dns = v["Self"]["DNSName"].as_str().unwrap_or_default().trim_end_matches('.').to_owned();
                    out.push(check(Status::Ok, "tailscale", format!("connected as {dns} ({}: {})", located.source, ts.display())));
                    let expected_url = format!("wss://{dns}/v1/ws");
                    match &config.server.public_url {
                        Some(u) if u == &expected_url => out.push(check(Status::Ok, "public url", u.clone())),
                        Some(u) => out.push(check(Status::Warn, "public url", format!("{u} (Tailscale suggests {expected_url})"))),
                        None => out.push(check(Status::Warn, "public url", format!("not set: add public_url = \"{expected_url}\" under [server]"))),
                    }
                    let serve = supervisor.run_tool(ToolSpec::new(&ts, &paths.data_dir).args(["serve", "status", "--json"])).await;
                    match serve {
                        Ok(o) if o.success() => {
                            // An empty configuration prints nothing (or `{}`).
                            let text = o.stdout_lossy();
                            let json = if text.trim().is_empty() { Ok(Value::Null) } else { serde_json::from_str::<Value>(&text).map_err(|e| e.to_string()) };
                            match json.and_then(|v| tailscale::serve_targets(&v)) {
                                Ok(entries) => out.extend(serve_checks(&entries, listen, admin)),
                                Err(e) => out.push(check(Status::Warn, "tailscale serve", format!("cannot read `tailscale serve status --json`: {e}"))),
                            }
                        }
                        Ok(o) => out.push(check(Status::Warn, "tailscale serve", format!("`tailscale serve status` failed: {}", o.stderr_lossy().trim()))),
                        Err(e) => out.push(check(Status::Warn, "tailscale serve", e.to_string())),
                    }
                }
                _ => out.push(check(Status::Warn, "tailscale", format!("installed ({}) but not connected (`tailscale up`)", ts.display()))),
            }
        }
    }

    match crate::autostart::status().await {
        Ok(tasks) => out.extend(autostart_checks(&tasks)),
        Err(e) => out.push(check(
            Status::Warn,
            "autostart",
            format!("cannot read the tasks: {e:#}"),
        )),
    }
    out
}

/// The checks of the autostart tasks (both registered, see design.md §18.3).
pub fn autostart_checks(
    tasks: &[(&'static str, Option<crate::autostart::TaskStatus>)],
) -> Vec<Check> {
    let missing: Vec<&str> = tasks
        .iter()
        .filter(|(_, s)| s.is_none())
        .map(|(name, _)| *name)
        .collect();
    if missing.len() == tasks.len() {
        return vec![check(
            Status::Warn,
            "autostart",
            "not installed: `agent-app-server autostart install`",
        )];
    }
    let mut out = Vec::new();
    for (name, status) in tasks {
        match status {
            Some(s) if s.enabled => out.push(check(Status::Ok, "autostart", s.describe())),
            Some(s) => out.push(check(
                Status::Warn,
                "autostart",
                format!(
                    "{} — enable it in Task Scheduler or run `agent-app-server autostart install`",
                    s.describe()
                ),
            )),
            None => out.push(check(
                Status::Warn,
                "autostart",
                format!(
                    "the task {name} is missing: run `agent-app-server autostart install` again"
                ),
            )),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(published: &str, mount: Option<&str>, host: &str, port: u16) -> ServeTarget {
        ServeTarget {
            published: published.into(),
            mount: mount.map(str::to_owned),
            host: host.into(),
            port,
            path: "/".into(),
        }
    }

    fn entries(targets: &[ServeTarget]) -> ServeEntries {
        ServeEntries {
            targets: targets.to_vec(),
            unreadable: Vec::new(),
        }
    }

    #[test]
    fn serve_checks_find_the_daemon_and_warn_about_the_admin_listener() {
        let listen: SocketAddr = "127.0.0.1:7878".parse().unwrap();
        let admin: SocketAddr = "127.0.0.1:7879".parse().unwrap();
        let ok = serve_checks(
            &entries(&[target(
                "https://pc.ts.net:443/",
                Some("/"),
                "127.0.0.1",
                7878,
            )]),
            listen,
            admin,
        );
        assert_eq!(ok.len(), 1);
        assert_eq!(ok[0].status, Status::Ok);
        let near_miss = serve_checks(
            &entries(&[target(
                "https://pc.ts.net:443/",
                Some("/"),
                "127.0.0.1",
                17878,
            )]),
            listen,
            admin,
        );
        assert_eq!(
            near_miss[0].status,
            Status::Warn,
            "a port containing ours is not ours"
        );
        let exposed = serve_checks(
            &entries(&[
                target("https://pc.ts.net:443/", Some("/"), "127.0.0.1", 7878),
                target("tcp:8443", None, "localhost", 7879),
            ]),
            listen,
            admin,
        );
        assert!(
            exposed
                .iter()
                .any(|c| c.status == Status::Warn && c.detail.contains("admin listener"))
        );
        assert!(exposed.iter().any(|c| c.status == Status::Ok));
        let tcp_only = serve_checks(
            &entries(&[target("tcp:443", None, "127.0.0.1", 7878)]),
            listen,
            admin,
        );
        assert_eq!(tcp_only[0].status, Status::Warn);
        assert!(
            serve_checks(&ServeEntries::default(), listen, admin)[0]
                .detail
                .contains("not publishing")
        );
        // A daemon published under a path does not serve its routes unchanged.
        let sub_path = ServeTarget {
            path: "/aas".into(),
            ..target("https://pc.ts.net:443/", Some("/"), "127.0.0.1", 7878)
        };
        assert_eq!(
            serve_checks(&entries(&[sub_path]), listen, admin)[0].status,
            Status::Warn
        );
        // The admin listener behind a path is still exposed.
        let admin_path = ServeTarget {
            path: "/v1".into(),
            ..target("https://pc.ts.net:443/x", Some("/x"), "127.0.0.1", 7879)
        };
        assert!(
            serve_checks(&entries(&[admin_path]), listen, admin)
                .iter()
                .any(|c| c.detail.contains("admin listener"))
        );
    }

    #[test]
    fn autostart_is_checked_task_by_task() {
        use crate::autostart::{KEEPALIVE_TASK_NAME, TASK_NAME, TaskStatus};
        let task = |name: &str, enabled: bool| TaskStatus {
            name: name.into(),
            enabled,
            state: "ready",
            last_run: None,
            last_result: crate::autostart::SCHED_S_TASK_HAS_NOT_RUN,
            next_run: None,
        };
        let none = autostart_checks(&[(TASK_NAME, None), (KEEPALIVE_TASK_NAME, None)]);
        assert_eq!(none.len(), 1);
        assert!(none[0].status == Status::Warn && none[0].detail.contains("not installed"));
        let both = autostart_checks(&[
            (TASK_NAME, Some(task(TASK_NAME, true))),
            (KEEPALIVE_TASK_NAME, Some(task(KEEPALIVE_TASK_NAME, true))),
        ]);
        assert!(both.iter().all(|c| c.status == Status::Ok), "{both:#?}");
        // An installation from before the keep-alive task, and a disabled task.
        let old = autostart_checks(&[
            (TASK_NAME, Some(task(TASK_NAME, false))),
            (KEEPALIVE_TASK_NAME, None),
        ]);
        assert!(old.iter().all(|c| c.status == Status::Warn), "{old:#?}");
        assert!(old.iter().any(|c| c.detail.contains("disabled")));
        assert!(old.iter().any(|c| c.detail.contains(KEEPALIVE_TASK_NAME)));
    }

    #[test]
    fn unreadable_entries_are_reported_next_to_every_other_check() {
        let listen: SocketAddr = "127.0.0.1:7878".parse().unwrap();
        let admin: SocketAddr = "127.0.0.1:7879".parse().unwrap();
        let checks = serve_checks(
            &ServeEntries {
                targets: vec![target("tcp:10000", None, "localhost", 7879)],
                unreadable: vec![
                    "https://pc.ts.net:443/sock: unix:/run/app.sock: unsupported scheme unix"
                        .into(),
                ],
            },
            listen,
            admin,
        );
        assert!(checks.iter().any(|c| {
            c.status == Status::Warn
                && c.detail
                    .contains("cannot read the entry https://pc.ts.net:443/sock")
        }));
        assert!(
            checks
                .iter()
                .any(|c| c.status == Status::Warn && c.detail.contains("admin listener")),
            "{checks:#?}"
        );
        assert!(
            checks.iter().any(|c| c.detail.contains("not publishing")),
            "{checks:#?}"
        );
    }
}
