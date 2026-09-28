//! `agent-app-server` — run the daemon in the foreground and manage it.

use std::path::PathBuf;

use aas_daemon::config::{Config, Paths};
use aas_daemon::doctor::Status;
use aas_daemon::{
    EXIT_CONFIG, EXIT_FAILURE, RunOptions, admin_client::AdminClient, autostart, doctor,
    load_or_init_config, qr,
};
use aas_protocol::http::{
    AdminDevicesResponse, AdminPairingCodeRequest, AdminPairingCodeResponse, AdminStatusResponse,
};
use aas_protocol::methods::{HarnessListResult, HarnessRefreshParams};
use anyhow::Context;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "agent-app-server",
    version,
    about = "Daemon that runs coding-agent CLIs for the phone app"
)]
struct Cli {
    /// Configuration folder (default: %APPDATA%\agent-app-server; or AAS_CONFIG_DIR).
    #[arg(long, global = true)]
    config_dir: Option<PathBuf>,
    /// Data folder (default: %LOCALAPPDATA%\agent-app-server; or AAS_DATA_DIR).
    #[arg(long, global = true)]
    data_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the daemon in this console (Ctrl+C stops it).
    Run {
        /// Started by the watchdog: log to files only, report readiness on stdout and follow
        /// the watchdog's control lines on stdin.
        #[arg(long)]
        background: bool,
    },
    /// Write the default configuration if it is missing and print its location.
    Init,
    /// Show a QR code for pairing the phone app.
    Pair,
    /// List paired devices.
    Devices,
    /// Revoke a paired device.
    Revoke { device_id: String },
    /// Show the running daemon's status.
    Status,
    /// Stop the running daemon.
    Stop {
        /// Wait for running turns to finish first.
        #[arg(long)]
        drain: bool,
    },
    /// Check the installation and connectivity.
    Doctor,
    /// Agent CLIs (harnesses) of the running daemon.
    Harness {
        #[command(subcommand)]
        action: HarnessAction,
    },
    /// Start the daemon at logon (Task Scheduler).
    Autostart {
        #[command(subcommand)]
        action: AutostartAction,
    },
    /// The fake harness's agent (for `kind = "fake"` with `command` set to this program and
    /// `args = ["fake"]`: exercises the client without spending tokens).
    #[command(hide = true)]
    Fake {
        #[command(subcommand)]
        what: FakeCommand,
    },
}

#[derive(Subcommand)]
enum FakeCommand {
    /// Speak the fake agent's JSON Lines protocol on stdin/stdout.
    Agent,
}

#[derive(Subcommand)]
enum HarnessAction {
    /// Probe the harnesses again (after installing or logging in to a CLI) and show them. The
    /// app is told about the result too.
    Refresh {
        /// Only this harness (its `id` in config.toml).
        harness_id: Option<String>,
    },
}

#[derive(Subcommand)]
enum AutostartAction {
    /// Register the logon and keep-alive tasks (and start the watchdog now).
    Install,
    /// Remove the tasks.
    Uninstall,
    /// Show whether the tasks exist, and their last and next runs.
    Status,
}

fn main() {
    let cli = Cli::parse();
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: cannot start the async runtime: {e}");
            std::process::exit(EXIT_FAILURE);
        }
    };
    let code = match runtime.block_on(real_main(cli)) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e:#}");
            EXIT_FAILURE
        }
    };
    // Exit without waiting for the runtime's blocking threads (e.g. the stdin reader).
    std::process::exit(code);
}

/// The configuration, or the defaults when it does not exist yet (the daemon writes it on its
/// first start).
fn config(paths: &Paths) -> anyhow::Result<Config> {
    let file = paths.config_file();
    if file.exists() {
        Config::load(&file)
    } else {
        Ok(Config::default())
    }
}

/// The admin client of the daemon of this configuration.
fn admin(paths: &Paths) -> anyhow::Result<AdminClient> {
    AdminClient::from_config(paths, &config(paths)?)
}

async fn run(paths: Paths, background: bool) -> i32 {
    let (config, created) = match load_or_init_config(&paths) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {e:#}");
            return EXIT_CONFIG;
        }
    };
    let _guard = match aas_daemon::init_logging(&paths, &config, !background) {
        Ok(guard) => guard,
        Err(e) => {
            eprintln!("error: cannot set up logging: {e:#}");
            return EXIT_CONFIG;
        }
    };
    if created {
        tracing::info!(file = %paths.config_file().display(), "wrote the initial configuration");
    }
    match aas_daemon::run_daemon(paths, config, RunOptions::new(background)).await {
        Ok(code) => code,
        Err(e) => {
            let code = e.exit_code();
            tracing::error!(error = %e, exit_code = code, "the daemon stopped with an error");
            eprintln!("error: {e}");
            code
        }
    }
}

async fn real_main(cli: Cli) -> anyhow::Result<i32> {
    let paths = match Paths::resolve(cli.config_dir, cli.data_dir) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: {e:#}");
            return Ok(EXIT_CONFIG);
        }
    };
    match cli.command {
        Command::Run { background } => Ok(run(paths, background).await),
        Command::Fake {
            what: FakeCommand::Agent,
        } => {
            let options = aas_adapter_fake::agent::AgentOptions {
                cwd: std::env::current_dir().context("the working folder")?,
                ..Default::default()
            };
            Ok(aas_adapter_fake::agent::run_stdio(options).await)
        }
        Command::Init => {
            let (config, created) = load_or_init_config(&paths)?;
            let file = paths.config_file();
            if created {
                println!("wrote {}", file.display());
            } else {
                println!("{} already exists", file.display());
            }
            println!(
                "harnesses: {}",
                config
                    .harnesses
                    .iter()
                    .map(|h| h.id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            println!(
                "project roots: {}",
                config
                    .projects
                    .roots
                    .iter()
                    .map(|r| r.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            println!(
                "listening on {} (admin {})",
                config.server.listen, config.server.admin_listen
            );
            Ok(0)
        }
        Command::Pair => {
            let client = admin(&paths)?;
            let r: AdminPairingCodeResponse = client
                .post("/v1/admin/pairing-codes", &AdminPairingCodeRequest {})
                .await?;
            println!("{}", qr::render(&r.pair_url)?);
            let minutes = (r.expires_at - now_ms()).max(0) / 60_000;
            println!("Scan with the app, or enter the code manually: {}", r.code);
            println!("The code expires in about {minutes} minute(s) and works once.");
            Ok(0)
        }
        Command::Devices => {
            let r: AdminDevicesResponse = admin(&paths)?.get("/v1/admin/devices").await?;
            if r.devices.is_empty() {
                println!("no paired devices (pair one with `agent-app-server pair`)");
            }
            for d in r.devices {
                println!(
                    "{}  {}  {}  last seen: {}",
                    d.id,
                    d.name,
                    d.platform.unwrap_or_default(),
                    d.last_seen_at
                        .map(format_ms)
                        .unwrap_or_else(|| "never".into())
                );
            }
            Ok(0)
        }
        Command::Revoke { device_id } => {
            admin(&paths)?
                .delete(&format!("/v1/admin/devices/{device_id}"))
                .await?;
            println!("revoked {device_id}");
            Ok(0)
        }
        Command::Status => {
            let client = admin(&paths)?;
            let s: AdminStatusResponse = client.get("/v1/admin/status").await?;
            println!("version:           {}", s.version);
            println!("listening on:      {}", s.listen);
            println!("admin listener:    {}", client.addr());
            println!(
                "public url:        {}",
                s.public_url.unwrap_or_else(|| "(not set)".into())
            );
            println!("uptime:            {}s", s.uptime_ms / 1000);
            println!("agent processes:   {}", s.running_processes);
            println!("running turns:     {}", s.running_turns);
            println!("connected devices: {}", s.connected_devices);
            println!("draining:          {}", s.draining);
            Ok(0)
        }
        Command::Stop { drain } => {
            admin(&paths)?
                .post_empty("/v1/admin/stop", serde_json::json!({ "drain": drain }))
                .await?;
            println!(
                "{}",
                if drain {
                    "stopping after running turns finish"
                } else {
                    "stopping"
                }
            );
            Ok(0)
        }
        Command::Doctor => {
            let checks = doctor::run(&paths).await;
            let mut failed = false;
            for c in &checks {
                let tag = match c.status {
                    Status::Ok => "OK  ",
                    Status::Warn => "WARN",
                    Status::Fail => {
                        failed = true;
                        "FAIL"
                    }
                };
                println!("[{tag}] {:<18} {}", c.name, c.detail);
            }
            Ok(if failed { 1 } else { 0 })
        }
        Command::Harness {
            action: HarnessAction::Refresh { harness_id },
        } => {
            let config = config(&paths)?;
            let r: HarnessListResult = AdminClient::from_config(&paths, &config)?
                .refresh_harnesses(
                    &HarnessRefreshParams { harness_id },
                    config.daemon_policy.harness_refresh_timeout,
                )
                .await?;
            for h in r.harnesses {
                let detail = if h.available {
                    format!(
                        "{}{}",
                        h.version.unwrap_or_default(),
                        h.executable.map(|e| format!(" ({e})")).unwrap_or_default()
                    )
                } else {
                    h.unavailable_reason.unwrap_or_default()
                };
                let state = if h.available {
                    "available"
                } else {
                    "unavailable"
                };
                println!("{:<12} {state:<12} {detail}", h.id);
            }
            Ok(0)
        }
        Command::Autostart { action } => match action {
            AutostartAction::Install => {
                let keepalive = config(&paths)?.daemon_policy.autostart_keepalive_interval;
                autostart::install(&paths, keepalive).await?;
                println!(
                    "installed and started: the daemon starts at logon, the watchdog restarts it after crashes, and the keep-alive task restarts the watchdog (every {keepalive:?}) unless it was stopped"
                );
                Ok(0)
            }
            AutostartAction::Uninstall => {
                autostart::uninstall().await?;
                println!(
                    "removed the tasks (a running daemon keeps running; stop it with `agent-app-server stop`)"
                );
                Ok(0)
            }
            AutostartAction::Status => {
                for (name, status) in autostart::status().await? {
                    match status {
                        Some(s) => println!("{}", s.describe()),
                        None => println!("{name}: not installed"),
                    }
                }
                Ok(0)
            }
        },
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn format_ms(ms: i64) -> String {
    let secs = (now_ms() - ms).max(0) / 1000;
    match secs {
        0..60 => format!("{secs}s ago"),
        60..3600 => format!("{}m ago", secs / 60),
        3600..86400 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86400),
    }
}
