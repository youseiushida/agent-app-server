//! Persistent record of spawned processes, used to clean up after a crash.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::orphans::{self, ProcessId, TreeSweep};

/// One spawned process.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LedgerEntry {
    pub pid: u32,
    /// OS creation time of the process (guards against PID reuse).
    pub created: u64,
    pub label: String,
    pub owner: Option<String>,
    pub spawned_at_ms: i64,
}

/// Outcome of [`crate::Supervisor::sweep_orphans`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SweepReport {
    /// Entries of which at least one process (the recorded process or a descendant) was
    /// still alive and has been terminated.
    pub terminated: Vec<LedgerEntry>,
    /// Descendants that were terminated with their recorded process, each with the PID of
    /// the ledger entry it belongs to.
    pub descendants: Vec<SweptDescendant>,
    /// Entries whose process was gone (or whose PID now belongs to another process), with no
    /// descendant left to terminate.
    pub already_gone: usize,
    /// Entries that could not be terminated, with the error.
    pub failed: Vec<(LedgerEntry, String)>,
}

/// A descendant of a ledger entry terminated by the sweep.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SweptDescendant {
    /// PID of the ledger entry (the root of the tree).
    pub root_pid: u32,
    pub process: ProcessId,
}

#[derive(Clone)]
pub(crate) struct Ledger {
    inner: Arc<Mutex<LedgerState>>,
}

struct LedgerState {
    path: PathBuf,
    entries: BTreeMap<u32, LedgerEntry>,
}

impl Ledger {
    pub fn open(path: PathBuf) -> std::io::Result<Self> {
        let entries = match std::fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<Vec<LedgerEntry>>(&bytes) {
                Ok(list) => list.into_iter().map(|e| (e.pid, e)).collect(),
                Err(e) => {
                    tracing::warn!(path = %path.display(), error = %e, "child ledger is unreadable; starting empty");
                    BTreeMap::new()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(e) => return Err(e),
        };
        Ok(Self {
            inner: Arc::new(Mutex::new(LedgerState { path, entries })),
        })
    }

    /// Records a spawned process and writes the ledger. On failure nothing is recorded (the
    /// file keeps its previous content) and the caller must not let the process run: after a
    /// crash nothing would find it.
    pub fn record(&self, entry: LedgerEntry) -> std::io::Result<()> {
        let mut state = self.inner.lock();
        let pid = entry.pid;
        let replaced = state.entries.insert(pid, entry);
        let written = state.persist();
        if written.is_err() {
            match replaced {
                Some(previous) => state.entries.insert(pid, previous),
                None => state.entries.remove(&pid),
            };
        }
        written
    }

    /// Forgets a process that has been reaped. A failed write only leaves a stale entry on
    /// disk: the next sweep finds its process gone (PID and creation time) and drops it.
    pub fn remove(&self, pid: u32) {
        let mut state = self.inner.lock();
        if state.entries.remove(&pid).is_some()
            && let Err(e) = state.persist()
        {
            tracing::error!(path = %state.path.display(), pid, error = %e, "failed to write the child ledger; the reaped process stays on it until the next sweep");
        }
    }

    #[cfg(test)]
    pub fn entries(&self) -> Vec<LedgerEntry> {
        self.inner.lock().entries.values().cloned().collect()
    }

    /// Terminates what is left of every recorded tree (see the `orphans` module), waiting up to
    /// `confirm_timeout` per tree for it to be gone, then clears the ledger.
    pub fn sweep(&self, confirm_timeout: Duration) -> SweepReport {
        let mut state = self.inner.lock();
        let mut report = SweepReport::default();
        for entry in std::mem::take(&mut state.entries).into_values() {
            let root = ProcessId {
                pid: entry.pid,
                created: entry.created,
            };
            match orphans::sweep_tree(root, confirm_timeout) {
                Ok(TreeSweep::Terminated {
                    root_running,
                    descendants,
                    unreadable,
                }) => {
                    for pid in &unreadable {
                        tracing::warn!(
                            pid,
                            root = entry.pid,
                            label = %entry.label,
                            "a child of a leftover process could not be inspected and was left alone"
                        );
                    }
                    if !root_running && descendants.is_empty() {
                        report.already_gone += 1;
                        continue;
                    }
                    tracing::warn!(
                        pid = entry.pid,
                        label = %entry.label,
                        root_running,
                        descendants = descendants.len(),
                        "terminated processes left over from a previous daemon run"
                    );
                    report
                        .descendants
                        .extend(descendants.into_iter().map(|process| SweptDescendant {
                            root_pid: entry.pid,
                            process,
                        }));
                    report.terminated.push(entry);
                }
                Ok(TreeSweep::Gone) => report.already_gone += 1,
                Err(e) => {
                    tracing::error!(pid = entry.pid, label = %entry.label, error = %e, "could not terminate leftover process tree");
                    report.failed.push((entry, e.to_string()));
                }
            }
        }
        if let Err(e) = state.persist() {
            // The swept entries stay on disk: the next sweep checks them again and finds them
            // gone (or terminates what is left), so nothing is lost but the log line.
            tracing::error!(path = %state.path.display(), error = %e, "failed to clear the child ledger after the sweep");
        }
        report
    }
}

impl LedgerState {
    /// Writes the entries to a temporary file and renames it over the ledger, so a crash never
    /// leaves a half-written ledger.
    fn persist(&self) -> std::io::Result<()> {
        let list: Vec<&LedgerEntry> = self.entries.values().collect();
        let bytes = serde_json::to_vec_pretty(&list).expect("ledger serializes");
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, bytes).and_then(|_| std::fs::rename(&tmp, &self.path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_persist_and_sweep_skips_foreign_processes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("children.json");
        let ledger = Ledger::open(path.clone()).unwrap();
        let me = std::process::id();
        let created = crate::platform::process_creation_time(me).unwrap();
        // Our own PID with a *different* creation time simulates PID reuse: must not be killed.
        ledger
            .record(LedgerEntry {
                pid: me,
                created: created + 7,
                label: "x".into(),
                owner: None,
                spawned_at_ms: 0,
            })
            .unwrap();
        ledger
            .record(LedgerEntry {
                pid: 3,
                created: 1,
                label: "gone".into(),
                owner: None,
                spawned_at_ms: 0,
            })
            .unwrap();

        let reopened = Ledger::open(path).unwrap();
        assert_eq!(reopened.entries().len(), 2);
        let report = reopened.sweep(Duration::from_secs(10));
        assert!(report.terminated.is_empty());
        assert!(report.descendants.is_empty());
        assert_eq!(report.already_gone, 2);
        assert!(reopened.entries().is_empty());
    }

    #[test]
    fn a_record_that_cannot_be_written_is_not_kept() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("children.json");
        let ledger = Ledger::open(path.clone()).unwrap();
        let entry = |pid: u32| LedgerEntry {
            pid,
            created: 1,
            label: format!("p{pid}"),
            owner: None,
            spawned_at_ms: 0,
        };
        ledger.record(entry(8)).unwrap();
        // A directory where the temporary file goes makes every write fail.
        std::fs::create_dir(path.with_extension("json.tmp")).unwrap();
        assert!(ledger.record(entry(12)).is_err());
        assert_eq!(
            ledger.entries(),
            vec![entry(8)],
            "the failed record is dropped"
        );
        let on_disk: Vec<LedgerEntry> =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            on_disk,
            vec![entry(8)],
            "the file keeps its previous content"
        );
    }
}
