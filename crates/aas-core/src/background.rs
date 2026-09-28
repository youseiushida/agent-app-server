//! The background tasks of a thread's agent process (design.md §5.6): what the adapter
//! reported last, what clients were told, and when a coalesced progress update or an
//! unconfirmed stop request is due.
//!
//! The thread actor owns one [`Background`] per process. Tasks are keyed by the harness's own
//! id (unique within a process); when the process ends every task that has not ended is ended
//! with it and the set starts empty for the next process, so ids of one process never meet
//! those of another.

use std::collections::HashMap;

use aas_harness::{BackgroundState, BackgroundTaskInfo};
use aas_protocol::{BackgroundTask, BackgroundTaskId, BackgroundTaskStatus};
use tokio::time::Instant;

/// One task as the actor tracks it.
pub(crate) struct Tracked {
    /// What the adapter reported last (its whole state).
    pub info: BackgroundTaskInfo,
    /// The task as clients see it once everything pending is written.
    pub view: BackgroundTask,
    /// The form last written to the log (what clients know now).
    pub written: Option<BackgroundTask>,
    /// When `written` was written (progress coalescing).
    pub written_at: Option<Instant>,
    /// A change of progress or usage only waits until then to be written
    /// (`policy.background_progress_interval`).
    pub flush_at: Option<Instant>,
    /// When a stop request counts as unconfirmed (`policy.background_stop_confirm_timeout`).
    pub stop_deadline: Option<Instant>,
}

impl Tracked {
    /// Whether the task keeps the agent busy: in the harness's live set and not ambient.
    pub fn keeps_busy(&self) -> bool {
        self.info.keeps_busy()
    }

    pub fn ended(&self) -> bool {
        self.view.status.is_terminal()
    }

    /// The earliest moment something of this task is due.
    fn deadline(&self) -> Option<Instant> {
        match (self.flush_at, self.stop_deadline) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }
}

/// The background tasks of one agent process.
#[derive(Default)]
pub(crate) struct Background {
    tasks: HashMap<String, Tracked>,
    keys: HashMap<BackgroundTaskId, String>,
}

impl Background {
    pub fn get(&self, key: &str) -> Option<&Tracked> {
        self.tasks.get(key)
    }

    pub fn get_mut(&mut self, key: &str) -> Option<&mut Tracked> {
        self.tasks.get_mut(key)
    }

    /// The harness's key of task `id` (of this process).
    pub fn key_of(&self, id: &BackgroundTaskId) -> Option<&str> {
        self.keys.get(id).map(String::as_str)
    }

    pub fn insert(&mut self, tracked: Tracked) {
        self.keys
            .insert(tracked.view.id.clone(), tracked.info.key.clone());
        self.tasks.insert(tracked.info.key.clone(), tracked);
    }

    /// Tasks that keep the agent busy (they block the idle stop, hold a sleep lease and hold
    /// up a drain).
    pub fn busy(&self) -> usize {
        self.tasks.values().filter(|t| t.keeps_busy()).count()
    }

    /// The task item `key` launched (the task names it as its origin).
    pub fn launched_by(&self, item_key: &str) -> Option<BackgroundTaskId> {
        self.tasks
            .values()
            .find(|t| t.info.origin_item_key.as_deref() == Some(item_key))
            .map(|t| t.view.id.clone())
    }

    /// When the next coalesced update or unconfirmed stop is due.
    pub fn next_deadline(&self) -> Option<Instant> {
        self.tasks.values().filter_map(Tracked::deadline).min()
    }

    /// Keys of the tasks whose deadline has come.
    pub fn due(&self, now: Instant) -> Vec<String> {
        self.tasks
            .values()
            .filter(|t| t.deadline().is_some_and(|d| d <= now))
            .map(|t| t.info.key.clone())
            .collect()
    }

    /// Keys of every task that has not ended.
    pub fn unfinished(&self) -> Vec<String> {
        let mut keys: Vec<(i64, String)> = self
            .tasks
            .values()
            .filter(|t| !t.ended())
            .map(|t| (t.view.started_at, t.info.key.clone()))
            .collect();
        keys.sort();
        keys.into_iter().map(|(_, k)| k).collect()
    }

    /// Forgets every task (the process is gone; what had not ended has been ended).
    pub fn clear(&mut self) {
        self.tasks.clear();
        self.keys.clear();
    }
}

/// The status a task has for the state the harness reported.
pub(crate) fn status_of(state: BackgroundState) -> BackgroundTaskStatus {
    match state {
        BackgroundState::Running => BackgroundTaskStatus::Running,
        BackgroundState::Completed => BackgroundTaskStatus::Completed,
        BackgroundState::Failed => BackgroundTaskStatus::Failed,
        BackgroundState::Stopped => BackgroundTaskStatus::Stopped,
    }
}

/// Whether `new` differs from `old` only in what a running task reports as it goes (its
/// progress and usage): such a change may wait (`policy.background_progress_interval`).
pub(crate) fn progress_only(old: &BackgroundTask, new: &BackgroundTask) -> bool {
    let running = |t: &BackgroundTask| t.status == BackgroundTaskStatus::Running;
    let rest = |t: &BackgroundTask| BackgroundTask {
        progress: None,
        usage: None,
        ..t.clone()
    };
    running(old) && running(new) && rest(old) == rest(new)
}

/// Whether the change from `old` to `new` changes the thread's summary
/// (`Thread.background`: running tasks that are not ambient, the task that ended last).
pub(crate) fn changes_summary(old: Option<&BackgroundTask>, new: &BackgroundTask) -> bool {
    match old {
        None => true,
        Some(old) => {
            old.status != new.status
                || old.ambient != new.ambient
                || old.ended_at != new.ended_at
                || (new.ended_at.is_some() && (old.title != new.title || old.kind != new.kind))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aas_protocol::examples;

    #[test]
    fn only_progress_and_usage_may_wait() {
        let running = examples::background_task();
        let progressed = BackgroundTask {
            progress: None,
            usage: Some(Default::default()),
            ..running.clone()
        };
        assert!(progress_only(&running, &progressed));
        let retitled = BackgroundTask {
            title: "other".into(),
            ..progressed.clone()
        };
        assert!(!progress_only(&running, &retitled));
        let ended = BackgroundTask {
            status: BackgroundTaskStatus::Completed,
            ..running.clone()
        };
        assert!(
            !progress_only(&running, &ended),
            "an end is never held back"
        );
        assert!(changes_summary(None, &running));
        assert!(!changes_summary(Some(&running), &progressed));
        assert!(changes_summary(Some(&running), &ended));
        assert!(changes_summary(
            Some(&running),
            &BackgroundTask {
                ambient: true,
                ..running.clone()
            }
        ));
    }

    #[test]
    fn deadlines_and_lookups() {
        let mut bg = Background::default();
        let now = Instant::now();
        let view = examples::background_task();
        let mut info = BackgroundTaskInfo::new(
            view.native_id.clone(),
            aas_protocol::BackgroundTaskKind::Agent,
            "t",
        );
        info.origin_item_key = Some("tool1".into());
        bg.insert(Tracked {
            info,
            view: view.clone(),
            written: None,
            written_at: None,
            flush_at: Some(now + std::time::Duration::from_secs(2)),
            stop_deadline: Some(now + std::time::Duration::from_secs(1)),
        });
        assert_eq!(bg.busy(), 1);
        assert_eq!(bg.launched_by("tool1"), Some(view.id.clone()));
        assert_eq!(bg.key_of(&view.id), Some(view.native_id.as_str()));
        assert_eq!(
            bg.next_deadline(),
            Some(now + std::time::Duration::from_secs(1))
        );
        assert!(bg.due(now).is_empty());
        assert_eq!(bg.due(now + std::time::Duration::from_secs(1)).len(), 1);
        assert_eq!(bg.unfinished(), vec![view.native_id.clone()]);
        bg.clear();
        assert_eq!(bg.busy(), 0);
        assert!(bg.next_deadline().is_none());
    }
}
