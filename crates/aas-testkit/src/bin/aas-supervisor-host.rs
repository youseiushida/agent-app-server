//! Stand-in daemon for tests. Tests kill this process and check that what it supervises dies
//! with it (Job Object `KILL_ON_JOB_CLOSE`).
//!
//! * `aas-supervisor-host <state-dir> <pid-dir> <depth> <width>` spawns `aas-dummy-agent tree`
//!   under a supervisor, prints `ready` once the whole tree has recorded itself, then waits
//!   forever.
//! * `aas-supervisor-host exec <state-dir> <cwd> -- <program> [args…]` spawns `program` under a
//!   supervisor, copies every line it writes to stdout to its own stdout, then waits forever
//!   (also after the program's stdout ends).

use std::path::PathBuf;
use std::time::Duration;

use aas_supervisor::{SpawnSpec, Supervisor, SupervisorPolicy};
use aas_testkit::proc;
use tokio::io::{AsyncBufReadExt, BufReader};

/// Upper bound for the tree to record itself before the host gives up (it panics).
const TREE_RECORD_TIMEOUT: Duration = Duration::from_secs(60);

fn supervisor(state: &std::path::Path) -> Supervisor {
    Supervisor::new(
        state,
        SupervisorPolicy {
            prevent_sleep: false,
            ..Default::default()
        },
    )
    .expect("supervisor")
}

async fn tree(args: &[String]) {
    let state = PathBuf::from(&args[1]);
    let pid_dir = PathBuf::from(&args[2]);
    let depth: u32 = args[3].parse().expect("depth");
    let width: u32 = args[4].parse().expect("width");
    let supervisor = supervisor(&state);
    let spec = SpawnSpec::new("tree", proc::bin_path("aas-dummy-agent"), &state).args([
        "tree".to_owned(),
        "--depth".into(),
        depth.to_string(),
        "--width".into(),
        width.to_string(),
        "--pid-dir".into(),
        pid_dir.display().to_string(),
    ]);
    let _child = supervisor.spawn(spec).await.expect("spawn tree");
    let expected = proc::tree_size(depth, width);
    tokio::task::spawn_blocking(move || {
        proc::wait_for_pids(&pid_dir, expected, TREE_RECORD_TIMEOUT)
    })
    .await
    .expect("wait");
    // `println!` flushes at the newline (stdout is line buffered), so the test sees it now.
    println!("ready");
    std::future::pending::<()>().await;
}

async fn exec(args: &[String]) {
    let usage = "usage: aas-supervisor-host exec <state-dir> <cwd> -- <program> [args…]";
    let (state, cwd) = (
        PathBuf::from(args.get(2).expect(usage)),
        PathBuf::from(args.get(3).expect(usage)),
    );
    assert_eq!(args.get(4).map(String::as_str), Some("--"), "{usage}");
    let program = args.get(5).expect(usage);
    let supervisor = supervisor(&state);
    let spec = SpawnSpec::new("exec", program, cwd).args(args[6..].iter().cloned());
    let mut child = supervisor.spawn(spec).await.expect("spawn the program");
    let stdout = child.stdout.take().expect("piped stdout");
    let mut lines = BufReader::new(stdout).lines();
    while let Some(line) = lines
        .next_line()
        .await
        .expect("reading the program's stdout")
    {
        println!("{line}");
    }
    std::future::pending::<()>().await;
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(async move {
        if args.get(1).map(String::as_str) == Some("exec") {
            exec(&args).await
        } else {
            tree(&args).await
        }
    });
}
