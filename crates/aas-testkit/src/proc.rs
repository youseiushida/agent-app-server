//! Process-tree helpers.
//!
//! Test processes (`aas-dummy-agent tree|linger`) record themselves in a directory: one file
//! per process, named by its PID and holding its creation time. Checks and cleanup compare
//! both, so a PID recycled by an unrelated process (Windows reuses PIDs quickly) is never
//! mistaken for — or killed as — the original process.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// How often the waiting helpers look again. A test helper's polling interval: short enough
/// not to slow tests down, long enough not to spin.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// A process identified by PID *and* creation time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Proc {
    pub pid: u32,
    /// OS creation time (as reported by [`aas_supervisor::process_creation_time`]).
    pub created: u64,
}

impl Proc {
    /// Identifies the process currently running under `pid`; `None` if there is none.
    pub fn of(pid: u32) -> Option<Self> {
        aas_supervisor::process_creation_time(pid).map(|created| Self { pid, created })
    }

    /// Whether this very process (not a later one with a recycled PID) is still running. A
    /// process that has exited counts as gone even while some handle keeps its object.
    pub fn alive(&self) -> bool {
        aas_supervisor::process_is_running(self.pid, self.created)
    }

    /// Terminates the process if it is still this process; returns whether it was terminated.
    pub fn terminate(&self) -> bool {
        aas_supervisor::terminate_if_same(self.pid, self.created).unwrap_or(false)
    }
}

/// Path of a binary built from this package (tests run from `target/<profile>/deps`).
pub fn bin_path(name: &str) -> PathBuf {
    let exe = std::env::current_exe().expect("current exe");
    let dir = exe.parent().expect("deps dir");
    let dir = if dir.ends_with("deps") {
        dir.parent().expect("profile dir")
    } else {
        dir
    };
    dir.join(if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_owned()
    })
}

/// Records the current process in `dir`. The record is written to a temporary name and
/// renamed, so readers never see a record without its creation time.
pub fn record_self(dir: &Path) -> std::io::Result<Proc> {
    std::fs::create_dir_all(dir)?;
    let me = Proc::of(std::process::id())
        .ok_or_else(|| std::io::Error::other("own creation time is unavailable"))?;
    let tmp = dir.join(format!("{}.tmp", me.pid));
    std::fs::write(&tmp, me.created.to_string())?;
    std::fs::rename(&tmp, dir.join(me.pid.to_string()))?;
    Ok(me)
}

/// Processes recorded in `dir`, sorted by PID (empty when the directory does not exist).
pub fn recorded(dir: &Path) -> Vec<Proc> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut procs: Vec<Proc> = entries
        .flatten()
        .filter_map(|entry| {
            let pid = entry.file_name().to_str()?.parse().ok()?;
            let created = std::fs::read_to_string(entry.path())
                .ok()?
                .trim()
                .parse()
                .ok()?;
            Some(Proc { pid, created })
        })
        .collect();
    procs.sort_unstable();
    procs
}

/// Waits until `dir` holds at least `count` records; panics after `timeout`.
pub fn wait_for_pids(dir: &Path, count: usize, timeout: Duration) -> Vec<Proc> {
    let deadline = Instant::now() + timeout;
    loop {
        let procs = recorded(dir);
        if procs.len() >= count {
            return procs;
        }
        assert!(
            Instant::now() < deadline,
            "only {} of {count} processes recorded themselves in {}",
            procs.len(),
            dir.display()
        );
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Waits until none of `procs` is alive; returns the survivors when `timeout` expires first.
pub fn wait_all_dead(procs: &[Proc], timeout: Duration) -> Vec<Proc> {
    let deadline = Instant::now() + timeout;
    loop {
        let alive: Vec<Proc> = procs.iter().copied().filter(Proc::alive).collect();
        if alive.is_empty() || Instant::now() >= deadline {
            return alive;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Number of processes in a full tree of `depth` levels below a root with `width` children
/// per node (root included).
pub fn tree_size(depth: u32, width: u32) -> usize {
    (0..=depth).map(|d| (width as usize).pow(d)).sum()
}

/// Terminates, when dropped, every process recorded in the registered directories plus the
/// explicitly registered processes. Tests keep one alive for their whole body so that a failed
/// assertion cannot leak processes living outside any job.
#[derive(Debug, Default)]
pub struct Cleanup {
    dirs: Vec<PathBuf>,
    procs: Vec<Proc>,
}

impl Cleanup {
    pub fn new() -> Self {
        Self::default()
    }

    /// Everything recorded in `dir` (now or later) is terminated on drop.
    pub fn dir(&mut self, dir: impl Into<PathBuf>) {
        self.dirs.push(dir.into());
    }

    /// `proc` is terminated on drop (if it is still that process).
    pub fn process(&mut self, proc: Proc) {
        self.procs.push(proc);
    }
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        let recorded = self.dirs.iter().flat_map(|d| recorded(d));
        for proc in self.procs.iter().copied().chain(recorded) {
            if proc.alive() && proc.terminate() {
                eprintln!("cleanup: terminated leftover test process {}", proc.pid);
            }
        }
    }
}
