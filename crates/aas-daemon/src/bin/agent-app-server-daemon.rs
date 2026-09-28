//! `agent-app-server-daemon` — the watchdog started at logon. It has no console window and
//! keeps `agent-app-server run --background` alive (see `aas_daemon::watchdog`).

#![cfg_attr(windows, windows_subsystem = "windows")]

use std::path::PathBuf;

use aas_daemon::config::Paths;

fn arg_value(args: &[String], name: &str) -> Option<PathBuf> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .map(PathBuf::from)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    // Without a console the exit code (and watchdog.log once the data folder is known) is the
    // only output.
    let paths = match Paths::resolve(
        arg_value(&args, "--config-dir"),
        arg_value(&args, "--data-dir"),
    ) {
        Ok(p) => p,
        Err(_) => std::process::exit(aas_daemon::EXIT_CONFIG),
    };
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(r) => r,
        Err(_) => std::process::exit(aas_daemon::EXIT_FAILURE),
    };
    // `--keepalive`: started by the keep-alive task (design.md §18.3).
    let keepalive = args
        .iter()
        .any(|a| a == aas_daemon::autostart::KEEPALIVE_ARG);
    let code = runtime.block_on(aas_daemon::watchdog::run(paths, keepalive));
    std::process::exit(code);
}
