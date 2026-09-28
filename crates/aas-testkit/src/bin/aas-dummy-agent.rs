//! Dummy agent for tests.
//!
//! * `aas-dummy-agent agent` — the fake agent speaking its JSON Lines protocol on stdio. When the
//!   environment variable `AAS_DUMMY_AGENT_PID_DIR` is set, the agent first records itself in
//!   that directory (so a test can check that no agent outlives its server).
//! * `aas-dummy-agent tree --depth D --width W --pid-dir DIR [--try-breakaway] [--say TEXT]` —
//!   records its PID in DIR, starts W children with depth D-1, then sleeps forever. With
//!   `--try-breakaway` the root also tries to start a child outside its job and records the
//!   outcome as `breakaway-ok` / `breakaway-refused` in DIR. With `--say` the root writes
//!   TEXT to stdout and stderr once its children are started (streaming tool tests).
//! * `aas-dummy-agent linger --pid-dir DIR` — starts one sleeping child, waits until the child
//!   has recorded itself in DIR, then exits with code 0 (the child lingers).
//! * `aas-dummy-agent job-probe` — for running inside another program's sandbox, where files
//!   may not be writable: tries to write `aas-job-probe.txt` in its working folder, starts one
//!   `sleep` child, tries to start another one outside its job (`CREATE_BREAKAWAY_FROM_JOB`),
//!   then prints one JSON line to stdout and sleeps forever:
//!   `{"probe":{"pid","created"},"child":{…},"wrote":bool,"writeError":…,"breakaway":"refused"|"allowed","breakawayError":…,"escaped":{…}}`
//!   (processes as PID and creation time; `escaped` is the child started outside the job, if
//!   any).
//! * `aas-dummy-agent sleep` — sleeps forever (records nothing).
//!
//! Every process of the `tree` and `linger` modes records itself in DIR (see
//! [`aas_testkit::proc::record_self`]).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use aas_testkit::proc;

/// Upper bound for a lingering child to record itself before `linger` gives up (exit code 1).
const CHILD_RECORD_TIMEOUT: Duration = Duration::from_secs(60);
/// How often `linger` looks for its child's record.
const CHILD_RECORD_POLL: Duration = Duration::from_millis(10);

/// Environment variable naming the directory in which an `agent` records itself.
const AGENT_PID_DIR_ENV: &str = "AAS_DUMMY_AGENT_PID_DIR";

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn record_pid(dir: &Path) {
    proc::record_self(dir).expect("record own pid");
}

fn spawn_self(args: &[&str]) -> std::io::Result<std::process::Child> {
    Command::new(std::env::current_exe()?)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
}

/// Writes `text` to stdout and stderr and flushes both.
fn say(text: &str) {
    use std::io::Write;
    let mut out = std::io::stdout();
    let mut err = std::io::stderr();
    if out
        .write_all(text.as_bytes())
        .and_then(|_| out.flush())
        .is_err()
        || err
            .write_all(text.as_bytes())
            .and_then(|_| err.flush())
            .is_err()
    {
        std::process::exit(1);
    }
}

/// One sleep of [`sleep_forever`]. A pure implementation interval: the loop never ends.
const SLEEP_CHUNK: Duration = Duration::from_secs(3600);

fn sleep_forever() -> ! {
    loop {
        std::thread::sleep(SLEEP_CHUNK);
    }
}

/// A process as the probe reports it.
fn proc_json(proc: Option<proc::Proc>) -> serde_json::Value {
    match proc {
        Some(p) => serde_json::json!({"pid": p.pid, "created": p.created}),
        None => serde_json::Value::Null,
    }
}

/// Starts `aas-dummy-agent sleep`, optionally with extra creation flags.
fn spawn_sleeper(creation_flags: u32) -> std::io::Result<std::process::Child> {
    let mut command = Command::new(std::env::current_exe()?);
    command
        .arg("sleep")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(creation_flags);
    }
    #[cfg(not(windows))]
    let _ = creation_flags;
    command.spawn()
}

/// `CREATE_BREAKAWAY_FROM_JOB`.
const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;

fn job_probe() -> ! {
    use std::io::Write;
    let write = std::fs::write("aas-job-probe.txt", b"probe");
    // The handles stay open for this process's life: the PIDs cannot be recycled meanwhile.
    let child = spawn_sleeper(0);
    let escaped = spawn_sleeper(CREATE_BREAKAWAY_FROM_JOB);
    let report = serde_json::json!({
        "probe": proc_json(proc::Proc::of(std::process::id())),
        "child": proc_json(child.as_ref().ok().and_then(|c| proc::Proc::of(c.id()))),
        "childError": child.as_ref().err().map(|e| e.to_string()),
        "wrote": write.is_ok(),
        "writeError": write.as_ref().err().map(|e| e.to_string()),
        "breakaway": if escaped.is_ok() { "allowed" } else { "refused" },
        "breakawayError": escaped.as_ref().err().map(|e| e.to_string()),
        "escaped": proc_json(escaped.as_ref().ok().and_then(|c| proc::Proc::of(c.id()))),
    });
    let mut out = std::io::stdout();
    if writeln!(out, "{report}").and_then(|_| out.flush()).is_err() {
        std::process::exit(1);
    }
    sleep_forever();
}

#[cfg(windows)]
fn try_breakaway(dir: &Path) {
    use std::os::windows::process::CommandExt;
    let exe = std::env::current_exe().expect("exe");
    let result = Command::new(exe)
        .args([
            "tree",
            "--depth",
            "0",
            "--width",
            "0",
            "--pid-dir",
            &dir.join("escaped").display().to_string(),
        ])
        .creation_flags(CREATE_BREAKAWAY_FROM_JOB)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    let marker = if result.is_ok() {
        "breakaway-ok"
    } else {
        "breakaway-refused"
    };
    std::fs::write(dir.join(marker), b"").expect("marker");
}

#[cfg(not(windows))]
fn try_breakaway(dir: &Path) {
    std::fs::write(dir.join("breakaway-refused"), b"").expect("marker");
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("agent") => {
            if let Some(dir) = std::env::var_os(AGENT_PID_DIR_ENV) {
                record_pid(Path::new(&dir));
            }
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("runtime");
            let options = aas_adapter_fake::agent::AgentOptions {
                cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
                chunk_size: 4,
            };
            let code = runtime.block_on(aas_adapter_fake::agent::run_stdio(options));
            std::process::exit(code);
        }
        Some("tree") => {
            let depth: u32 = arg(&args, "--depth")
                .and_then(|d| d.parse().ok())
                .unwrap_or(0);
            let width: u32 = arg(&args, "--width")
                .and_then(|w| w.parse().ok())
                .unwrap_or(0);
            let dir = PathBuf::from(arg(&args, "--pid-dir").expect("--pid-dir"));
            record_pid(&dir);
            // The child handles live as long as this process (it sleeps forever below); the
            // whole tree is torn down by the Job Object under test, which is what tests observe.
            let mut children = Vec::new();
            if depth > 0 {
                let d = (depth - 1).to_string();
                let w = width.to_string();
                let dir_s = dir.display().to_string();
                for _ in 0..width {
                    children.push(
                        spawn_self(&["tree", "--depth", &d, "--width", &w, "--pid-dir", &dir_s])
                            .expect("spawn child"),
                    );
                }
            }
            if args.iter().any(|a| a == "--try-breakaway") {
                try_breakaway(&dir);
            }
            if let Some(text) = arg(&args, "--say") {
                say(&text);
            }
            sleep_forever();
        }
        Some("job-probe") => job_probe(),
        Some("sleep") => sleep_forever(),
        Some("linger") => {
            let dir = PathBuf::from(arg(&args, "--pid-dir").expect("--pid-dir"));
            record_pid(&dir);
            let child = spawn_self(&[
                "tree",
                "--depth",
                "0",
                "--width",
                "0",
                "--pid-dir",
                &dir.display().to_string(),
            ])
            .expect("spawn child");
            let record = dir.join(child.id().to_string());
            drop(child);
            // Exit only once the child is on record, so the test knows exactly what lingers.
            let deadline = Instant::now() + CHILD_RECORD_TIMEOUT;
            while !record.exists() {
                if Instant::now() >= deadline {
                    eprintln!("linger: the child never recorded itself");
                    std::process::exit(1);
                }
                std::thread::sleep(CHILD_RECORD_POLL);
            }
            std::process::exit(0);
        }
        _ => {
            eprintln!(
                "usage: aas-dummy-agent agent | tree --depth D --width W --pid-dir DIR [--try-breakaway] [--say TEXT] | linger --pid-dir DIR | job-probe | sleep"
            );
            std::process::exit(2);
        }
    }
}
