//! Long-running server-side operations (`git clone`): progress relay, cancellation and the
//! final outcome.
//!
//! A clone writes into a temporary sibling of its target (`.<name>.aas-clone-<operation>`) and
//! is renamed to the target only when it succeeded, so a failed or cancelled clone never leaves
//! a half-written target folder. The temporary folder is recorded with the operation (removed
//! at startup when the daemon stopped during the clone).
//!
//! Progress: git's output is split into display lines ([`crate::progress`]); the latest line
//! is published as `Operation.progress` through `operation/updated`. Updates are coalesced by
//! availability: the next update goes out once the previous one is committed and
//! `policy.operation_progress_interval` has passed, carrying whatever line is the latest then.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use aas_protocol::events::Event;
use aas_protocol::methods::ProjectCreateResult;
use aas_protocol::{Operation, OperationId, OperationStatus};
use aas_supervisor::{ToolCancel, ToolChunk};
use parking_lot::Mutex;
use tokio::sync::{mpsc, watch};

use crate::error::CoreResult;
use crate::git::{CloneError, Git};
use crate::progress::ProgressLines;
use crate::shared::{Idem, Shared};
use crate::store::{self, now_ms};

/// Message of an operation that ended because the daemon stopped.
pub const DAEMON_STOPPED: &str = "the daemon stopped during this operation";

/// A running operation, as seen by `operation/cancel` and the daemon's shutdown.
#[derive(Clone)]
struct Running {
    cancel: ToolCancel,
    /// Set when the cancellation comes from the daemon's shutdown (the outcome is `failed`,
    /// not `cancelled`: nobody asked for it).
    shutdown: Arc<AtomicBool>,
    /// Becomes `true` once the final state is committed.
    done: watch::Receiver<bool>,
}

/// Operations that currently run in this process.
#[derive(Default)]
pub struct OperationRegistry {
    running: Mutex<HashMap<OperationId, Running>>,
}

impl OperationRegistry {
    /// Cancels `id` if it runs here and waits until its final state is committed. Returns
    /// `false` when it does not run (finished, or unknown).
    pub async fn cancel(&self, id: &OperationId) -> bool {
        let Some(running) = self.running.lock().get(id).cloned() else {
            return false;
        };
        running.cancel.cancel();
        wait_done(running.done).await;
        true
    }

    /// Stops every running operation (daemon shutdown) and waits for their final states.
    pub async fn shutdown(&self) {
        let all: Vec<Running> = self.running.lock().values().cloned().collect();
        for r in &all {
            r.shutdown.store(true, Ordering::SeqCst);
            r.cancel.cancel();
        }
        futures::future::join_all(all.into_iter().map(|r| wait_done(r.done))).await;
    }
}

async fn wait_done(mut done: watch::Receiver<bool>) {
    // The sender is dropped only after `true` was sent (or by a panicking task, which also
    // means nothing more will happen): either way the wait is over.
    let _ = done.wait_for(|d| *d).await;
}

/// The temporary sibling a clone of `target` writes into.
pub fn clone_work_dir(target: &Path, op: &OperationId) -> Option<PathBuf> {
    let name = target.file_name()?.to_string_lossy().into_owned();
    Some(target.with_file_name(format!(
        ".{name}.aas-clone-{}",
        op.as_str().to_ascii_lowercase()
    )))
}

/// What a clone needs besides the engine state.
pub struct CloneJob {
    pub op: Operation,
    pub url: String,
    pub target: PathBuf,
    pub work_dir: PathBuf,
    pub name: String,
    pub git: Git,
}

/// Records the new operation (with the idempotent result of `project/create`), registers it
/// and starts the clone in the background.
pub async fn start_clone(sh: &Arc<Shared>, job: CloneJob, idem: Option<Idem>) -> CoreResult<()> {
    let cancel = ToolCancel::new();
    let shutdown = Arc::new(AtomicBool::new(false));
    let (done_tx, done_rx) = watch::channel(false);
    // Registered before the row is committed: a cancel that follows the response always finds
    // the running operation.
    sh.operations.running.lock().insert(
        job.op.id.clone(),
        Running {
            cancel: cancel.clone(),
            shutdown: shutdown.clone(),
            done: done_rx,
        },
    );
    let (op_row, work) = (job.op.clone(), job.work_dir.display().to_string());
    let stored = ProjectCreateResult {
        project: None,
        operation: Some(job.op.clone()),
    };
    let recorded = sh
        .tx(move |tx, em| {
            store::insert_operation(tx, &op_row, Some(&work))?;
            em.workspace(Event::OperationUpdated { operation: op_row });
            if let Some(idem) = &idem {
                idem.store_result(tx, &stored)?;
            }
            Ok(())
        })
        .await;
    if let Err(e) = recorded {
        sh.operations.running.lock().remove(&job.op.id);
        return Err(e);
    }
    let sh = sh.clone();
    tokio::spawn(async move {
        let id = job.op.id.clone();
        run_clone(&sh, job, &cancel, &shutdown).await;
        sh.operations.running.lock().remove(&id);
        done_tx.send_replace(true);
    });
    Ok(())
}

/// How a clone ended, before it is recorded.
enum Outcome {
    /// The clone is at its target.
    Cloned,
    Failed(String),
    Cancelled,
}

async fn run_clone(sh: &Arc<Shared>, job: CloneJob, cancel: &ToolCancel, shutdown: &AtomicBool) {
    let (tx, rx) = mpsc::unbounded_channel();
    let pump = tokio::spawn(relay_progress(sh.clone(), job.op.id.clone(), rx));
    let result = job
        .git
        .clone_repo(&job.url, &job.work_dir, tx, cancel)
        .await;
    // The pump ends when git's output ends (the sender is gone): no progress update can land
    // after the final state below.
    if let Err(e) = pump.await {
        tracing::error!(operation = %job.op.id, error = %e, "the progress relay failed");
    }
    let mut outcome = match result {
        Ok(()) if cancel.is_cancelled() => Outcome::Cancelled,
        Ok(()) => match move_into_place(&job.work_dir, &job.target) {
            Ok(()) => Outcome::Cloned,
            Err(message) => Outcome::Failed(message),
        },
        Err(CloneError::Cancelled) => Outcome::Cancelled,
        Err(CloneError::Failed(message)) => Outcome::Failed(message),
    };
    // Whatever did not become the project is removed. When that fails the folder stays
    // recorded (the next start tries again) and the message says where it is.
    let mut leftover = None;
    if !matches!(outcome, Outcome::Cloned)
        && job.work_dir.exists()
        && let Err(e) = remove_dir_all(&job.work_dir)
    {
        tracing::warn!(operation = %job.op.id, folder = %job.work_dir.display(), error = %e, "could not remove the partial clone");
        leftover = Some(format!(
            "the partial clone in {} could not be removed: {e}",
            job.work_dir.display()
        ));
    }
    if matches!(outcome, Outcome::Cancelled) && shutdown.load(Ordering::SeqCst) {
        outcome = Outcome::Failed(DAEMON_STOPPED.into());
    }
    record_outcome(sh, job, outcome, leftover).await;
}

/// Renames the finished clone to its target. The target is checked again: something may have
/// created it while the clone ran.
fn move_into_place(work: &Path, target: &Path) -> Result<(), String> {
    if target.exists() {
        return Err(format!(
            "{} was created while cloning; the clone was discarded",
            target.display()
        ));
    }
    std::fs::rename(work, target)
        .map_err(|e| format!("the clone could not be moved to {}: {e}", target.display()))
}

/// `std::fs::remove_dir_all`, clearing read-only attributes (git's pack files are read-only)
/// when the first attempt is refused.
pub fn remove_dir_all(dir: &Path) -> std::io::Result<()> {
    match std::fs::remove_dir_all(dir) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            clear_readonly(dir)?;
            std::fs::remove_dir_all(dir)
        }
        Err(e) => Err(e),
    }
}

fn clear_readonly(dir: &Path) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let meta = entry.metadata()?;
        if meta.is_dir() {
            clear_readonly(&entry.path())?;
        } else if meta.permissions().readonly() {
            let mut perms = meta.permissions();
            // The daemon targets Windows, where this clears only the read-only attribute of a
            // file about to be deleted (git pack files); the lint is about Unix, where it would
            // make the file world-writable.
            #[allow(clippy::permissions_set_readonly_false)]
            perms.set_readonly(false);
            std::fs::set_permissions(entry.path(), perms)?;
        }
    }
    Ok(())
}

async fn record_outcome(
    sh: &Arc<Shared>,
    job: CloneJob,
    outcome: Outcome,
    leftover: Option<String>,
) {
    let CloneJob {
        mut op,
        target,
        name,
        ..
    } = job;
    op.finished_at = Some(now_ms());
    op.progress = None;
    let id = op.id.clone();
    let failure = match outcome {
        Outcome::Cloned => {
            let (path, done) = (target.clone(), op.clone());
            let registered = sh
                .tx_durable("the end of an operation", move |tx, em| {
                    // Same find-or-revive as `project/open`: the folder may belong to a removed
                    // (or concurrently opened) project.
                    let project = crate::engine::upsert_project(tx, em, &path, name.clone())?;
                    let mut done = done.clone();
                    done.status = OperationStatus::Succeeded;
                    done.project_id = Some(project.id.clone());
                    done.message = Some("Cloned".into());
                    store::upsert_operation(tx, &done)?;
                    store::clear_operation_work_dir(tx, &done.id)?;
                    em.workspace(Event::OperationUpdated { operation: done });
                    Ok(())
                })
                .await;
            match registered {
                Ok(()) => return,
                Err(e) => Some((
                    OperationStatus::Failed,
                    format!(
                        "the repository was cloned to {} but could not be registered: {e}",
                        target.display()
                    ),
                )),
            }
        }
        Outcome::Failed(message) => Some((OperationStatus::Failed, message)),
        Outcome::Cancelled => Some((OperationStatus::Cancelled, "Cancelled".to_owned())),
    };
    let Some((status, message)) = failure else {
        return;
    };
    let keep_work_dir = leftover.is_some();
    let message = match leftover {
        Some(note) => format!("{message} ({note})"),
        None => message,
    };
    // The operation must never stay `running` (a failure here takes the fail-stop path, and
    // the restart records it as failed).
    let result = sh
        .tx_durable("the end of an operation", move |tx, em| {
            let mut op = op.clone();
            op.status = status;
            op.message = Some(message.clone());
            store::upsert_operation(tx, &op)?;
            if !keep_work_dir {
                store::clear_operation_work_dir(tx, &op.id)?;
            }
            em.workspace(Event::OperationUpdated { operation: op });
            Ok(())
        })
        .await;
    if let Err(e) = result {
        tracing::error!(operation = %id, error = %e, "recording the end of the operation failed");
    }
}

/// Publishes the latest progress line of a running operation (see the module docs).
async fn relay_progress(
    sh: Arc<Shared>,
    id: OperationId,
    mut rx: mpsc::UnboundedReceiver<ToolChunk>,
) {
    let interval = sh.config.policy.operation_progress_interval;
    let mut lines = ProgressLines::new(sh.config.policy.max_progress_line_bytes);
    let mut pending: Option<String> = None;
    let mut next_allowed = tokio::time::Instant::now();
    loop {
        tokio::select! {
            biased;
            chunk = rx.recv() => match chunk {
                Some(chunk) => {
                    if let Some(line) = lines.push(&chunk.bytes) {
                        pending = Some(line);
                    }
                }
                // The tool has ended: its final state follows, a late line would only flicker.
                None => return,
            },
            _ = tokio::time::sleep_until(next_allowed), if pending.is_some() => {
                let Some(line) = pending.take() else { continue };
                if let Err(e) = publish_progress(&sh, &id, line).await {
                    tracing::warn!(operation = %id, error = %e, "publishing clone progress failed");
                }
                next_allowed = tokio::time::Instant::now() + interval;
            }
        }
    }
}

async fn publish_progress(sh: &Shared, id: &OperationId, line: String) -> CoreResult<()> {
    let id = id.clone();
    sh.tx_durable("operation progress", move |tx, em| {
        let Some(mut op) = store::get_operation(tx, &id)? else {
            return Ok(());
        };
        if op.status != OperationStatus::Running || op.progress.as_deref() == Some(line.as_str()) {
            return Ok(());
        }
        op.progress = Some(line.clone());
        store::upsert_operation(tx, &op)?;
        em.workspace(Event::OperationUpdated { operation: op });
        Ok(())
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_work_dir_is_a_hidden_sibling_of_the_target() {
        let target = Path::new("C:\\Users\\me\\Documents\\app");
        let work = clone_work_dir(target, &OperationId::from("op_01ABC")).unwrap();
        assert_eq!(work.parent(), target.parent());
        assert_eq!(
            work.file_name().unwrap().to_string_lossy(),
            ".app.aas-clone-op_01abc"
        );
    }

    #[test]
    fn remove_dir_all_handles_read_only_files_and_missing_folders() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("partial");
        std::fs::create_dir_all(root.join("objects/pack")).unwrap();
        let pack = root.join("objects/pack/pack-1.idx");
        std::fs::write(&pack, b"x").unwrap();
        let mut perms = std::fs::metadata(&pack).unwrap().permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&pack, perms).unwrap();
        remove_dir_all(&root).unwrap();
        assert!(!root.exists());
        remove_dir_all(&root).unwrap();
    }
}
