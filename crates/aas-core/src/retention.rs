//! Retention: what is deleted, when, and how the space comes back (design.md §6.1).
//!
//! * Event log: deltas of completed items (`delta_retention`), events a later event of the
//!   same entity carries in full, a background task's output deltas among them
//!   (`superseded_event_retention`), `native` and
//!   `composer/insert` events (`native_event_retention`). Everything is deleted in batches of
//!   `maintenance_batch_size`, one short transaction each.
//! * Blobs: references are recorded explicitly when a blob is attached, spilled or produced
//!   (`blob_refs`, see `store`); a blob without references is deleted once it has been
//!   unreferenced for `unreferenced_blob_grace`.
//! * Removed threads (with their project): everything stored about them is purged in the
//!   removing transaction (`store::purge_thread`); what lives outside the database (snapshot
//!   refs, worktrees the daemon created) becomes cleanup jobs, run right away and retried
//!   here until they succeed.
//! * Finished operations are forgotten after `finished_operation_retention`; idempotency
//!   records and pairing codes expire as before.
//! * The space is returned to the file system with `incremental_vacuum`.

use std::panic::Location;
use std::path::Path;
use std::sync::Arc;

use aas_protocol::BlobId;

use crate::blobs::CollectionGuard;
use crate::config::path_within;
use crate::error::{CoreError, CoreResult};
use crate::shared::Shared;
use crate::store::{self, CleanupJob, CleanupKind, now_ms};

/// What one maintenance pass did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MaintenanceReport {
    /// `item/delta` events of completed items deleted.
    pub deltas: usize,
    /// Events deleted because a later event of the same entity carries their content
    /// (background task output deltas included).
    pub superseded: usize,
    /// `native` events deleted.
    pub native: usize,
    /// Expired idempotency records deleted.
    pub idempotency_records: usize,
    /// Expired or used pairing codes deleted.
    pub pairing_codes: usize,
    /// Finished operations forgotten.
    pub operations: usize,
    /// Unreferenced blobs deleted, and their bytes.
    pub blobs: usize,
    pub blob_bytes: u64,
    /// Cleanup jobs completed, and those still pending (they are retried next time).
    pub cleanup_done: usize,
    pub cleanup_pending: usize,
    /// Database pages returned to the file system.
    pub vacuumed_pages: u64,
}

fn cutoff(retention: std::time::Duration) -> aas_protocol::Millis {
    now_ms().saturating_sub(i64::try_from(retention.as_millis()).unwrap_or(i64::MAX))
}

/// Runs `step` with a limit of `batch` (one transaction each) until it reports that nothing
/// more is left. `step` returns how many rows it deleted and whether there may be more. The
/// transactions are recorded as the caller's (`Db::write_in_progress`).
#[track_caller]
fn in_batches<F>(
    sh: &Shared,
    batch: usize,
    step: F,
) -> impl Future<Output = CoreResult<usize>> + Send + '_
where
    F: Fn(&rusqlite::Transaction<'_>, usize) -> CoreResult<(usize, bool)>
        + Send
        + Sync
        + Clone
        + 'static,
{
    let caller = Location::caller();
    async move {
        let mut total = 0;
        loop {
            let step = step.clone();
            let (deleted, more) = sh
                .db
                .clone()
                .write_at(caller, move |tx| step(tx, batch))
                .await?;
            total += deleted;
            if !more {
                return Ok(total);
            }
        }
    }
}

/// One maintenance pass. Every step runs even when an earlier one failed (a failed step is
/// retried at the next pass; nothing is lost by waiting); the first error is returned.
pub(crate) async fn run(sh: &Arc<Shared>) -> CoreResult<MaintenanceReport> {
    let _running = sh.retention.lock().await;
    let policy = sh.config.policy.clone();
    let batch = policy.maintenance_batch_size;
    let mut report = MaintenanceReport::default();
    let mut first_error: Option<CoreError> = None;
    let mut note = |step: &str, result: CoreResult<()>| {
        if let Err(e) = result {
            tracing::error!(step, error = %e, "maintenance step failed; retried at the next pass");
            first_error.get_or_insert(e);
        }
    };

    let deltas_before = cutoff(policy.delta_retention);
    let r = in_batches(sh, batch, move |tx, limit| {
        let deleted = aas_eventlog::compact_deltas(tx, deltas_before, limit)?;
        Ok((deleted, deleted == limit))
    })
    .await;
    note("deltas", r.map(|n| report.deltas = n));

    let superseded_before = cutoff(policy.superseded_event_retention);
    let r = in_batches(sh, batch, move |tx, limit| {
        let deleted = aas_eventlog::compact_superseded(tx, superseded_before, limit)?;
        Ok((deleted, deleted == limit))
    })
    .await;
    note("superseded events", r.map(|n| report.superseded = n));

    // A background task's output deltas are superseded by a later update of the task (its
    // output so far, or its end): kept as long as the other superseded events.
    let r = in_batches(sh, batch, move |tx, limit| {
        let deleted = aas_eventlog::compact_background_output(tx, superseded_before, limit)?;
        Ok((deleted, deleted == limit))
    })
    .await;
    note(
        "background output deltas",
        r.map(|n| report.superseded += n),
    );

    let native_before = cutoff(policy.native_event_retention);
    for type_name in aas_eventlog::TRANSIENT_EVENT_TYPES {
        let r = in_batches(sh, batch, move |tx, limit| {
            let deleted = aas_eventlog::compact_type(tx, type_name, native_before, limit)?;
            Ok((deleted, deleted == limit))
        })
        .await;
        note("transient events", r.map(|n| report.native += n));
    }

    let idem_before = cutoff(policy.idempotency_ttl);
    let ops_before = cutoff(policy.finished_operation_retention);
    let r = sh
        .db
        .write(move |tx| {
            Ok((
                store::idem_gc(tx, idem_before)?,
                store::gc_pairing_codes(tx, now_ms())?,
                store::delete_finished_operations(tx, ops_before)?,
            ))
        })
        .await;
    note(
        "expired records",
        r.map(|(idem, codes, ops)| {
            report.idempotency_records = idem;
            report.pairing_codes = codes;
            report.operations = ops;
        }),
    );

    let r = collect_blobs(sh).await;
    note(
        "blobs",
        r.map(|(n, bytes)| {
            report.blobs = n;
            report.blob_bytes = bytes;
        }),
    );

    let r = run_cleanup_jobs(sh, None).await;
    note(
        "cleanup jobs",
        r.map(|(done, pending)| {
            report.cleanup_done = done;
            report.cleanup_pending = pending;
        }),
    );

    let r = sh
        .db
        .incremental_vacuum(policy.incremental_vacuum_pages)
        .await;
    note("vacuum", r.map(|n| report.vacuumed_pages = n));

    match first_error {
        Some(e) => Err(e),
        None => Ok(report),
    }
}

/// Deletes blobs that have been unreferenced for `unreferenced_blob_grace` (their record and
/// their file), skipping pinned ones. Returns how many and their bytes.
async fn collect_blobs(sh: &Arc<Shared>) -> CoreResult<(usize, u64)> {
    let before = cutoff(sh.config.policy.unreferenced_blob_grace);
    let batch = sh.config.policy.maintenance_batch_size;
    let mut total = (0usize, 0u64);
    loop {
        let sh2 = sh.clone();
        let (found, deleted, bytes) =
            tokio::task::spawn_blocking(move || -> CoreResult<(usize, usize, u64)> {
                // Storing and pinning wait while this runs (see `blobs`), and the candidates are
                // selected in the transaction that deletes them: nothing can store, pin or refer
                // to one of them in between.
                let guard = sh2.blobs.lock_for_collection();
                sh2.db.write_blocking(|tx| {
                    let candidates = store::expired_blobs(tx, before, batch)?;
                    let (deleted, bytes) = delete_unreferenced(tx, &guard, &candidates, before)?;
                    Ok((candidates.len(), deleted, bytes))
                })
            })
            .await
            .map_err(|e| CoreError::Internal(format!("blob collection task failed: {e}")))??;
        total.0 += deleted;
        total.1 += bytes;
        if found < batch || deleted == 0 {
            return Ok(total);
        }
    }
}

/// Deletes the blobs among `candidates` that are not pinned and, checked again here, still
/// unreferenced with a grace period that began before `before`: first the record, then the
/// file, so a blob that was referred to or stored again since it became a candidate keeps
/// both. A file that cannot be removed (e.g. open for a download) gets its record back (the
/// savepoint is rolled back) and the next pass tries again. Returns how many were deleted and
/// their bytes. Runs under the collection lock (`guard`) inside the writer's transaction.
fn delete_unreferenced(
    tx: &rusqlite::Transaction<'_>,
    guard: &CollectionGuard<'_>,
    candidates: &[BlobId],
    before: aas_protocol::Millis,
) -> CoreResult<(usize, u64)> {
    let mut deleted = (0usize, 0u64);
    for id in candidates.iter().filter(|id| !guard.is_pinned(id)) {
        tx.execute_batch("SAVEPOINT collect_blob")?;
        let Some(size) = store::delete_expired_blob(tx, id, before)? else {
            // Referred to, or stored again, since it was selected: kept.
            tx.execute_batch("RELEASE collect_blob")?;
            continue;
        };
        match guard.remove_file(id) {
            Ok(()) => {
                tx.execute_batch("RELEASE collect_blob")?;
                deleted.0 += 1;
                deleted.1 += size;
            }
            Err(e) => {
                tracing::warn!(blob = %id, error = %e, "could not delete an unreferenced blob; retried later");
                tx.execute_batch("ROLLBACK TO collect_blob; RELEASE collect_blob")?;
            }
        }
    }
    Ok(deleted)
}

/// Runs the pending cleanup jobs (only `only` when given). Returns how many were completed
/// and how many are still pending (of those run). The caller holds `Shared::retention`.
pub(crate) async fn run_cleanup_jobs(
    sh: &Arc<Shared>,
    only: Option<&[i64]>,
) -> CoreResult<(usize, usize)> {
    let jobs: Vec<CleanupJob> = sh
        .db
        .read(|tx| store::cleanup_jobs(tx))
        .await?
        .into_iter()
        .filter(|j| only.is_none_or(|ids| ids.contains(&j.id)))
        .collect();
    let (mut done, mut pending) = (0, 0);
    for job in jobs {
        let outcome = run_cleanup_job(sh, &job).await;
        let id = job.id;
        let message = match outcome {
            JobOutcome::Done => {
                sh.db
                    .write(move |tx| store::finish_cleanup_job(tx, id))
                    .await?;
                done += 1;
                continue;
            }
            JobOutcome::Waiting(message) => {
                // An expected state (a drive that is not connected, a repository moved away):
                // reported once, then only in the job's record.
                if job.attempts == 0 {
                    tracing::info!(job = id, kind = ?job.kind, target = %job.target, reason = %message, "cleanup job waits for its repository");
                } else {
                    tracing::debug!(job = id, kind = ?job.kind, target = %job.target, reason = %message, "cleanup job still waits for its repository");
                }
                message
            }
            JobOutcome::Failed(message) => {
                tracing::warn!(job = id, kind = ?job.kind, target = %job.target, attempts = job.attempts + 1, error = %message, "cleanup job failed; retried at the next maintenance");
                message
            }
        };
        sh.db
            .write(move |tx| store::fail_cleanup_job(tx, id, &message))
            .await?;
        pending += 1;
    }
    Ok((done, pending))
}

/// How one run of a cleanup job ended. Anything but `Done` keeps the job, which maintenance
/// runs again.
#[derive(Debug, Clone, PartialEq, Eq)]
enum JobOutcome {
    Done,
    /// The repository cannot be reached now (see [`run_cleanup_job`]).
    Waiting(String),
    Failed(String),
}

impl From<Result<(), String>> for JobOutcome {
    fn from(r: Result<(), String>) -> Self {
        match r {
            Ok(()) => JobOutcome::Done,
            Err(e) => JobOutcome::Failed(e),
        }
    }
}

/// Runs one cleanup job.
///
/// A repository that cannot be found at its path right now is not taken as gone for good: a
/// drive that is not connected, a network share that is offline and a repository that was
/// moved all look the same, and nothing tells them apart. Its refs may come back with it,
/// and a worktree may hold the only copy of uncommitted work, which only git can check (and
/// only with the repository). So the job waits, and nothing is deleted, until the repository
/// is reachable again; a worktree folder is only ever removed by `git worktree remove` without
/// force. A worktree whose folder no longer exists (the user deleted it) needs nothing more.
async fn run_cleanup_job(sh: &Shared, job: &CleanupJob) -> JobOutcome {
    let repo = Path::new(&job.repo);
    let reachable = repo.is_dir() && crate::git::quick_info(repo).is_repo;
    let waiting = || {
        JobOutcome::Waiting(format!(
            "the repository {} cannot be reached; nothing is deleted until it can",
            repo.display()
        ))
    };
    let git = || {
        sh.git
            .as_ref()
            .ok_or_else(|| JobOutcome::Failed("git is not available".into()))
    };
    match job.kind {
        CleanupKind::SnapshotRefs => {
            if !reachable {
                return waiting();
            }
            let git = match git() {
                Ok(g) => g,
                Err(outcome) => return outcome,
            };
            git.drop_snapshots(repo, &job.target)
                .await
                .map_err(|e| e.to_string())
                .into()
        }
        CleanupKind::Branch => {
            if !reachable {
                return waiting();
            }
            let git = match git() {
                Ok(g) => g,
                Err(outcome) => return outcome,
            };
            git.branch_delete(repo, &job.target)
                .await
                .map_err(|e| e.to_string())
                .into()
        }
        CleanupKind::Worktree => {
            let path = Path::new(&job.target);
            if !path_within(path, &sh.config.worktrees_dir()) {
                // Only folders the daemon created are ever removed.
                return JobOutcome::Failed(format!(
                    "{} is not a worktree the daemon created",
                    path.display()
                ));
            }
            let exists = path.exists();
            if !reachable {
                if exists {
                    return waiting();
                }
                // Nothing of it is left on disk; git forgets its record of the worktree by
                // itself (`git worktree prune`, also run by `git gc`).
                remove_empty_parent(path, &sh.config.worktrees_dir());
                return JobOutcome::Done;
            }
            let git = match git() {
                Ok(g) => g,
                Err(outcome) => return outcome,
            };
            let removed = if exists {
                // Not forced: the worktree was clean when its thread was removed; if it has
                // changes now, they are kept and the job reports why.
                git.worktree_remove(repo, path, false).await
            } else {
                git.worktree_prune(repo).await
            };
            if let Err(e) = removed {
                return JobOutcome::Failed(e.to_string());
            }
            remove_empty_parent(path, &sh.config.worktrees_dir());
            JobOutcome::Done
        }
    }
}

/// Removes the per-project folder of `path` under `root` once it is empty.
pub(crate) fn remove_empty_parent(path: &Path, root: &Path) {
    let Some(parent) = path.parent() else { return };
    if parent == root || !path_within(parent, root) {
        return;
    }
    match std::fs::remove_dir(parent) {
        Ok(()) => {}
        // Still used by another worktree, or already gone.
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::DirectoryNotEmpty | std::io::ErrorKind::NotFound
            ) => {}
        Err(e) => {
            tracing::warn!(folder = %parent.display(), error = %e, "could not remove an empty worktree folder")
        }
    }
}

/// Startup: deletes what a previous run left behind outside the database — temporary files
/// (spills, snapshot indexes) and blob files without a record (written by a transaction that
/// failed). Nothing of this run exists yet.
pub(crate) async fn sweep_leftovers(sh: &Arc<Shared>) -> CoreResult<()> {
    let removed = sh.blobs.clear_tmp()?;
    if removed > 0 {
        tracing::info!(removed, "removed temporary files left by the previous run");
    }
    let stored = sh.blobs.stored_ids()?;
    if stored.is_empty() {
        return Ok(());
    }
    let ids = stored.clone();
    let known = sh.db.read(move |tx| store::known_blobs(tx, &ids)).await?;
    let guard = sh.blobs.lock_for_collection();
    let mut removed = 0;
    for id in stored.iter().filter(|id| !known.contains(*id)) {
        match guard.remove_file(id) {
            Ok(()) => removed += 1,
            Err(e) => {
                tracing::warn!(blob = %id, error = %e, "could not remove a blob file without a record")
            }
        }
    }
    if removed > 0 {
        tracing::info!(removed, "removed blob files without a record");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blobs::BlobStore;
    use crate::db::Db;

    /// A blob stored and recorded at `at`, unreferenced since then.
    fn stored(db: &Db, blobs: &BlobStore, content: &[u8], at: aas_protocol::Millis) -> BlobId {
        let (id, size, pin) = blobs.put_bytes(content).unwrap();
        let (row, size) = (id.clone(), size);
        db.write_blocking(|tx| store::insert_blob(tx, &row, "image/png", size, at))
            .unwrap();
        drop(pin);
        id
    }

    #[test]
    fn a_blob_used_again_since_it_was_selected_keeps_its_file() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_in_memory().unwrap();
        let blobs = BlobStore::new(dir.path().join("blobs"), dir.path().join("tmp"));
        let long_ago = 1_000;
        let before = 2_000;
        let expired = stored(&db, &blobs, b"nobody wants this", long_ago);
        let revived = stored(&db, &blobs, b"uploaded again", long_ago);
        // Selected as candidates, then — before the collection — the same photo is uploaded
        // again, which starts its grace period anew.
        let candidates = db
            .write_blocking(|tx| store::expired_blobs(tx, before, 10))
            .unwrap();
        assert_eq!(candidates.len(), 2);
        db.write_blocking(|tx| store::insert_blob(tx, &revived, "image/png", 14, now_ms()))
            .unwrap();
        let guard = blobs.lock_for_collection();
        let (deleted, _) = db
            .write_blocking(|tx| delete_unreferenced(tx, &guard, &candidates, before))
            .unwrap();
        drop(guard);
        assert_eq!(deleted, 1);
        assert!(!blobs.path_of(&expired).unwrap().exists());
        assert!(
            blobs.path_of(&revived).unwrap().exists(),
            "the file of a blob that is used again stays"
        );
        let known = db
            .write_blocking(|tx| store::known_blobs(tx, &[expired.clone(), revived.clone()]))
            .unwrap();
        assert!(known.contains(&revived) && !known.contains(&expired));
    }

    #[test]
    fn a_file_that_cannot_be_removed_keeps_its_record() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_in_memory().unwrap();
        let blobs = BlobStore::new(dir.path().join("blobs"), dir.path().join("tmp"));
        let id = stored(&db, &blobs, b"being downloaded", 1_000);
        // Something that `remove_file` cannot delete sits where the file is.
        let path = blobs.path_of(&id).unwrap();
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        let guard = blobs.lock_for_collection();
        let candidates = db
            .write_blocking(|tx| store::expired_blobs(tx, 2_000, 10))
            .unwrap();
        let (deleted, _) = db
            .write_blocking(|tx| delete_unreferenced(tx, &guard, &candidates, 2_000))
            .unwrap();
        drop(guard);
        assert_eq!(deleted, 0);
        let known = db
            .write_blocking(|tx| store::known_blobs(tx, std::slice::from_ref(&id)))
            .unwrap();
        assert!(known.contains(&id), "the record stays for the next pass");
    }
}
