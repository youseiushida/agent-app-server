//! Terminating the process tree of a ledger entry left over from a previous daemon run.
//!
//! A ledger entry names only the process the supervisor spawned. Its descendants are found
//! through the parent PIDs of a system process snapshot. A parent PID is the PID the parent had
//! when the child was created, and Windows reuses PIDs quickly, so a snapshot entry is taken as
//! a child only when both hold:
//!
//! * its parent is a process already attributed to the tree, identified by PID *and* creation
//!   time and held open while the walk runs (an open handle keeps the PID from being given to
//!   another process), and
//! * it was created no earlier than that parent. A process whose recorded parent PID matches
//!   but that is older than the parent is the child of an earlier process that had the same PID.
//!
//! The root itself must still match the PID and creation time on the ledger. Nothing is
//! inferred beyond these two facts: a descendant whose own parent has exited (and whose PID
//! object is gone) cannot be attributed and is left alone (see `docs/design.md` §4.5).
//!
//! On Windows the attributed processes are put into a fresh Job Object and terminated with
//! `TerminateJobObject`. Anything a member starts after it joined the job belongs to the job
//! too, so a process started while the walk runs cannot escape; the job's process count then
//! confirms that the whole tree is gone.

use std::collections::{HashSet, VecDeque};

/// A process identified by PID and creation time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ProcessId {
    pub pid: u32,
    /// OS creation time (as reported by [`crate::process_creation_time`]).
    pub created: u64,
}

/// Outcome of one attribution pass over a process snapshot.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Attribution {
    /// Descendants found in this pass, each after its parent.
    pub found: Vec<ProcessId>,
    /// Processes whose parent PID names a member but that are older than that member: the PID
    /// was recycled, so they belong to someone else.
    pub recycled: Vec<ProcessId>,
    /// PIDs listed as children of a member whose creation time could not be read (they
    /// exited meanwhile, or they cannot be opened).
    pub unreadable: Vec<u32>,
}

/// Walks `snapshot` (`(pid, parent pid)` pairs) from `members` and returns the descendants not
/// in `known`. `created_of` reads the creation time of a live process.
pub(crate) fn attribute(
    members: &[ProcessId],
    known: &HashSet<u32>,
    snapshot: &[(u32, u32)],
    mut created_of: impl FnMut(u32) -> Option<u64>,
) -> Attribution {
    let mut out = Attribution::default();
    let mut seen: HashSet<u32> = known.clone();
    seen.extend(members.iter().map(|m| m.pid));
    let mut queue: VecDeque<ProcessId> = members.iter().copied().collect();
    while let Some(parent) = queue.pop_front() {
        for &(pid, ppid) in snapshot {
            // PID 0 (the idle process) lists itself as its own parent.
            if ppid != parent.pid || pid == ppid || seen.contains(&pid) {
                continue;
            }
            seen.insert(pid);
            match created_of(pid) {
                None => out.unreadable.push(pid),
                Some(created) if created >= parent.created => {
                    let child = ProcessId { pid, created };
                    out.found.push(child);
                    queue.push_back(child);
                }
                Some(created) => out.recycled.push(ProcessId { pid, created }),
            }
        }
    }
    out
}

/// What happened to the tree of one ledger entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TreeSweep {
    /// The root is gone, or its PID now belongs to another process.
    Gone,
    /// The root (if still running) and every attributed descendant were terminated.
    Terminated {
        /// Whether the root itself was still running.
        root_running: bool,
        /// Descendants that were running and have been terminated.
        descendants: Vec<ProcessId>,
        /// Children of a member that could not be inspected (and were therefore left alone).
        unreadable: Vec<u32>,
    },
}

#[cfg(windows)]
pub(crate) use windows_impl::sweep_tree;

#[cfg(windows)]
mod windows_impl {
    use std::collections::{HashMap, HashSet};
    use std::time::{Duration, Instant};

    use super::{ProcessId, TreeSweep, attribute};
    use crate::platform::{OwnedProcess, TreeJob, process_parents};

    /// Terminates the tree rooted at `root` (if the root still matches) and waits up to
    /// `confirm_timeout` for all of it to be gone.
    pub(crate) fn sweep_tree(
        root: ProcessId,
        confirm_timeout: Duration,
    ) -> std::io::Result<TreeSweep> {
        let Some(root_handle) = OwnedProcess::open(root.pid) else {
            return Ok(TreeSweep::Gone);
        };
        if root_handle.created != root.created {
            return Ok(TreeSweep::Gone);
        }
        let root_running = root_handle.is_running();
        let job = TreeJob::new()?;
        // Members are held open for the whole walk: their PIDs cannot be recycled meanwhile.
        let mut members: HashMap<u32, OwnedProcess> = HashMap::new();
        // Members that could not join the job; terminated one by one.
        let mut outside: Vec<u32> = Vec::new();
        let join = |process: &OwnedProcess, outside: &mut Vec<u32>| -> std::io::Result<()> {
            if !process.is_running() || job.contains(process)? {
                return Ok(());
            }
            if let Err(e) = job.assign(process) {
                tracing::warn!(pid = process.pid, error = %e, "cannot put a leftover process into a job; terminating it directly");
                outside.push(process.pid);
            }
            Ok(())
        };
        join(&root_handle, &mut outside)?;
        members.insert(root.pid, root_handle);

        let mut descendants = Vec::new();
        let mut unreadable = Vec::new();
        let mut frontier = vec![root];
        // Every pass looks for children created before their parent joined the job (later ones
        // are in the job already). The set of such processes only shrinks, so this ends.
        while !frontier.is_empty() {
            let snapshot = process_parents()?;
            let known: HashSet<u32> = members.keys().copied().collect();
            let mut opened: HashMap<u32, OwnedProcess> = HashMap::new();
            let found = attribute(&frontier, &known, &snapshot, |pid| {
                let process = OwnedProcess::open(pid)?;
                let created = process.created;
                opened.insert(pid, process);
                Some(created)
            });
            for recycled in &found.recycled {
                tracing::debug!(
                    pid = recycled.pid,
                    "a process with a recycled parent PID is not part of the tree"
                );
            }
            unreadable.extend(found.unreadable.iter().copied());
            frontier = Vec::new();
            for child in found.found {
                let Some(process) = opened.remove(&child.pid) else {
                    continue;
                };
                if process.is_running() {
                    descendants.push(child);
                }
                join(&process, &mut outside)?;
                members.insert(child.pid, process);
                frontier.push(child);
            }
        }

        let deadline = Instant::now() + confirm_timeout;
        job.terminate()?;
        for pid in &outside {
            if let Some(process) = members.get(pid) {
                process.terminate()?;
            }
        }
        // The job's process count covers what joined it without being seen; the handles of
        // the attributed processes confirm each of them (their objects are signalled only
        // once they are fully gone).
        let mut confirmed = job.wait_empty(confirm_timeout)?;
        for process in members.values() {
            confirmed &= process.wait_exit(deadline.saturating_duration_since(Instant::now()));
        }
        if !confirmed {
            return Err(std::io::Error::other(format!(
                "processes of the tree of {} were still alive {confirm_timeout:?} after termination",
                root.pid
            )));
        }
        Ok(TreeSweep::Terminated {
            root_running,
            descendants,
            unreadable,
        })
    }
}

#[cfg(unix)]
pub(crate) use unix_impl::sweep_tree;

#[cfg(unix)]
mod unix_impl {
    use std::collections::HashSet;
    use std::time::Duration;

    use super::{ProcessId, TreeSweep, attribute};
    use crate::platform::{process_creation_time, process_is_running};

    /// `(pid, parent pid)` of every process in `/proc`.
    fn process_parents() -> std::io::Result<Vec<(u32, u32)>> {
        let mut out = Vec::new();
        for entry in std::fs::read_dir("/proc")?.flatten() {
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|n| n.parse::<u32>().ok())
            else {
                continue;
            };
            let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
                continue;
            };
            let Some((_, after_comm)) = stat.rsplit_once(')') else {
                continue;
            };
            if let Some(ppid) = after_comm
                .split_whitespace()
                .nth(1)
                .and_then(|p| p.parse::<u32>().ok())
            {
                out.push((pid, ppid));
            }
        }
        Ok(out)
    }

    /// Unix has no handle that pins a PID, so the walk is a single pass over one snapshot and
    /// every process is re-checked (PID and start time) right before it is signalled.
    pub(crate) fn sweep_tree(
        root: ProcessId,
        _confirm_timeout: Duration,
    ) -> std::io::Result<TreeSweep> {
        if !process_is_running(root.pid, root.created) {
            return Ok(TreeSweep::Gone);
        }
        let found = attribute(
            &[root],
            &HashSet::new(),
            &process_parents()?,
            process_creation_time,
        );
        let mut targets = vec![root];
        targets.extend(found.found.iter().copied());
        let mut descendants = Vec::new();
        for target in targets {
            if crate::platform::terminate_if_same(target.pid, target.created)? && target != root {
                descendants.push(target);
            }
        }
        Ok(TreeSweep::Terminated {
            root_running: true,
            descendants,
            unreadable: found.unreadable,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(pid: u32, created: u64) -> ProcessId {
        ProcessId { pid, created }
    }

    #[test]
    fn descendants_are_found_level_by_level() {
        // 100 → 200 → 300, 100 → 201; 999 is unrelated.
        let snapshot = [(100, 4), (200, 100), (201, 100), (300, 200), (999, 4)];
        let times = [(200, 20), (201, 21), (300, 30), (999, 1)];
        let found = attribute(&[p(100, 10)], &HashSet::new(), &snapshot, |pid| {
            times.iter().find(|(p, _)| *p == pid).map(|(_, t)| *t)
        });
        assert_eq!(found.found, vec![p(200, 20), p(201, 21), p(300, 30)]);
        assert!(found.recycled.is_empty());
        assert!(found.unreadable.is_empty());
    }

    #[test]
    fn a_child_older_than_its_parent_belongs_to_a_recycled_pid() {
        // 300 names 200 as its parent but was created before the current 200: it is the child
        // of an earlier process that had PID 200, and so is its own child 400.
        let snapshot = [(200, 100), (300, 200), (400, 300)];
        let times = [(200, 20), (300, 15), (400, 40)];
        let found = attribute(&[p(100, 10)], &HashSet::new(), &snapshot, |pid| {
            times.iter().find(|(p, _)| *p == pid).map(|(_, t)| *t)
        });
        assert_eq!(found.found, vec![p(200, 20)]);
        assert_eq!(found.recycled, vec![p(300, 15)]);
    }

    #[test]
    fn unreadable_and_known_processes_are_not_walked() {
        let snapshot = [(200, 100), (201, 100), (300, 201), (0, 0)];
        let found = attribute(&[p(100, 10)], &HashSet::from([201]), &snapshot, |pid| {
            (pid != 200).then_some(50)
        });
        assert!(found.found.is_empty(), "{found:?}");
        assert_eq!(found.unreadable, vec![200]);
        // The idle process (PID 0, its own parent) never loops the walk.
        let idle = attribute(&[p(0, 0)], &HashSet::new(), &[(0, 0)], |_| Some(0));
        assert_eq!(idle, Attribution::default());
    }
}
