//! Per-connection request ordering.
//!
//! Requests that touch the same thread (or project) run one after another in arrival order;
//! everything else runs concurrently. A lane is a FIFO worker per key; lanes live as long as
//! the connection.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

use aas_protocol::ClientRequest;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

type Job = Pin<Box<dyn Future<Output = ()> + Send>>;

#[derive(Default)]
pub(crate) struct Lanes {
    lanes: HashMap<String, (mpsc::UnboundedSender<Job>, JoinHandle<()>)>,
}

/// The ordering key of a request.
pub(crate) fn key_of(req: &ClientRequest) -> Option<String> {
    use ClientRequest as R;
    let thread = |id: &aas_protocol::ThreadId| Some(format!("thread:{id}"));
    let project = |id: &aas_protocol::ProjectId| Some(format!("project:{id}"));
    match req {
        R::ThreadUpdate(p) => thread(&p.thread_id),
        R::ThreadArchive(p) => thread(&p.thread_id),
        R::ThreadFork(p) => thread(&p.thread_id),
        R::ThreadStop(p) => thread(&p.thread_id),
        R::TurnStart(p) => thread(&p.thread_id),
        R::TurnInterrupt(p) => thread(&p.thread_id),
        R::QueueRemove(p) => thread(&p.thread_id),
        R::QueueResume(p) => thread(&p.thread_id),
        R::QueueUpdate(p) => thread(&p.thread_id),
        R::QueueSteer(p) => thread(&p.thread_id),
        R::BackgroundTaskStop(p) => thread(&p.thread_id),
        R::ItemMoveToBackground(p) => thread(&p.thread_id),
        R::ThreadCreate(p) => project(&p.project_id),
        R::ProjectUpdate(p) => project(&p.project_id),
        R::ProjectArchive(p) => project(&p.project_id),
        R::ProjectRemove(p) => project(&p.project_id),
        _ => None,
    }
}

impl Lanes {
    /// Runs `job` after earlier jobs of the same key; unkeyed jobs start immediately.
    pub fn submit(&mut self, key: Option<String>, job: impl Future<Output = ()> + Send + 'static) {
        let Some(key) = key else {
            tokio::spawn(job);
            return;
        };
        let (tx, _) = self.lanes.entry(key).or_insert_with(|| {
            let (tx, mut rx) = mpsc::unbounded_channel::<Job>();
            let worker = tokio::spawn(async move {
                while let Some(job) = rx.recv().await {
                    job.await;
                }
            });
            (tx, worker)
        });
        let _ = tx.send(Box::pin(job));
    }

    /// Stops accepting jobs; queued jobs still run.
    pub fn close(&mut self) {
        self.lanes.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;

    #[tokio::test]
    async fn same_key_runs_in_order() {
        let log = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let mut lanes = Lanes::default();
        for i in 0..5u64 {
            let log = log.clone();
            lanes.submit(Some("t".into()), async move {
                tokio::time::sleep(Duration::from_millis(10 * (5 - i))).await;
                log.lock().push(i);
            });
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(*log.lock(), vec![0, 1, 2, 3, 4]);
    }
}
