//! SQLite access: one writer connection (serialized), a pool of readers, WAL mode.
//! All calls run on the blocking pool.

use std::panic::Location;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex};
use rusqlite::{Connection, Transaction, TransactionBehavior};

use crate::error::{CoreError, CoreResult};

const SCHEMA_VERSION: i64 = 8;

/// SQLite's `auto_vacuum` value for incremental vacuuming (free pages are kept until
/// `PRAGMA incremental_vacuum` returns them).
const AUTO_VACUUM_INCREMENTAL: i64 = 2;

const SCHEMA_V1: &str = r#"
CREATE TABLE meta (
    key TEXT PRIMARY KEY NOT NULL,
    value TEXT NOT NULL
) STRICT;

CREATE TABLE devices (
    id TEXT PRIMARY KEY NOT NULL,
    name TEXT NOT NULL,
    platform TEXT,
    token_hash TEXT NOT NULL UNIQUE,
    created_at INTEGER NOT NULL,
    last_seen_at INTEGER,
    revoked_at INTEGER
) STRICT;

CREATE TABLE pairing_codes (
    code_hash TEXT PRIMARY KEY NOT NULL,
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL,
    used_at INTEGER
) STRICT;

CREATE TABLE projects (
    id TEXT PRIMARY KEY NOT NULL,
    name TEXT NOT NULL,
    path TEXT NOT NULL,
    path_key TEXT NOT NULL UNIQUE,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    archived INTEGER NOT NULL DEFAULT 0,
    removed INTEGER NOT NULL DEFAULT 0,
    defaults TEXT NOT NULL
) STRICT;

CREATE TABLE threads (
    id TEXT PRIMARY KEY NOT NULL,
    project_id TEXT NOT NULL,
    harness_id TEXT NOT NULL,
    title TEXT NOT NULL,
    title_source TEXT NOT NULL,
    cwd TEXT NOT NULL,
    workspace TEXT NOT NULL,
    settings TEXT NOT NULL,
    status TEXT NOT NULL,
    native_session_id TEXT,
    fork_source TEXT,
    forked_from TEXT,
    last_error TEXT,
    usage TEXT NOT NULL,
    base_tree TEXT,
    diff_available INTEGER NOT NULL DEFAULT 0,
    queue_paused INTEGER NOT NULL DEFAULT 0,
    head INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    last_activity_at INTEGER NOT NULL,
    archived INTEGER NOT NULL DEFAULT 0,
    removed INTEGER NOT NULL DEFAULT 0
) STRICT;
CREATE INDEX threads_activity ON threads(removed, archived, last_activity_at DESC, id DESC);
CREATE INDEX threads_project ON threads(project_id, last_activity_at DESC);

CREATE TABLE turns (
    id TEXT PRIMARY KEY NOT NULL,
    thread_id TEXT NOT NULL,
    idx INTEGER NOT NULL,
    status TEXT NOT NULL,
    started_at INTEGER NOT NULL,
    completed_at INTEGER,
    model TEXT,
    error TEXT,
    usage TEXT,
    diff TEXT,
    base_tree TEXT,
    end_tree TEXT,
    UNIQUE (thread_id, idx)
) STRICT;

CREATE TABLE items (
    id TEXT PRIMARY KEY NOT NULL,
    thread_id TEXT NOT NULL,
    turn_id TEXT NOT NULL,
    ord INTEGER NOT NULL,
    status TEXT NOT NULL,
    started_at INTEGER NOT NULL,
    completed_at INTEGER,
    body TEXT NOT NULL
) STRICT;
CREATE INDEX items_turn ON items(turn_id, ord);

CREATE TABLE interactions (
    id TEXT PRIMARY KEY NOT NULL,
    thread_id TEXT NOT NULL,
    turn_id TEXT,
    item_id TEXT,
    status TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    resolved_at INTEGER,
    resolved_by TEXT,
    request TEXT NOT NULL,
    resolution TEXT,
    expire_reason TEXT,
    adapter_request_id TEXT NOT NULL
) STRICT;
CREATE INDEX interactions_thread ON interactions(thread_id, created_at);
CREATE INDEX interactions_pending ON interactions(status) WHERE status = 'pending';

CREATE TABLE queued_inputs (
    id TEXT PRIMARY KEY NOT NULL,
    thread_id TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    input TEXT NOT NULL
) STRICT;
CREATE INDEX queued_thread ON queued_inputs(thread_id, created_at);

CREATE TABLE blobs (
    id TEXT PRIMARY KEY NOT NULL,
    mime TEXT NOT NULL,
    size INTEGER NOT NULL,
    created_at INTEGER NOT NULL
) STRICT;

CREATE TABLE operations (
    id TEXT PRIMARY KEY NOT NULL,
    kind TEXT NOT NULL,
    status TEXT NOT NULL,
    project_id TEXT,
    message TEXT,
    started_at INTEGER NOT NULL,
    finished_at INTEGER
) STRICT;

CREATE TABLE idempotency (
    device_id TEXT NOT NULL,
    client_request_id TEXT NOT NULL,
    method TEXT NOT NULL,
    params_hash TEXT NOT NULL,
    response TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (device_id, client_request_id)
) STRICT;
"#;

/// v2: pinned threads, operation progress, and the working folder of an operation (removed
/// when the operation ends, or at startup when the daemon stopped during it).
const SCHEMA_V2: &str = r#"
ALTER TABLE threads ADD COLUMN pinned INTEGER NOT NULL DEFAULT 0;
ALTER TABLE operations ADD COLUMN progress TEXT;
ALTER TABLE operations ADD COLUMN work_dir TEXT;
"#;

/// v3: retention (design.md §6.1). Blob references are recorded explicitly (`blob_refs`,
/// kept in step with the items and queued inputs that hold them); `blobs.orphaned_at` is when
/// a blob lost its last reference (or was stored without one), `NULL` while referenced.
/// `cleanup_jobs` holds the file-system and git work of a removal that is still to be done
/// (retried by maintenance until it succeeds). The event log gains its key columns
/// (`aas_eventlog::migrate`).
const SCHEMA_V3: &str = r#"
ALTER TABLE blobs ADD COLUMN orphaned_at INTEGER;
CREATE INDEX blobs_orphaned ON blobs(orphaned_at) WHERE orphaned_at IS NOT NULL;

CREATE TABLE blob_refs (
    blob_id TEXT NOT NULL,
    owner_kind TEXT NOT NULL,
    owner_id TEXT NOT NULL,
    thread_id TEXT NOT NULL,
    PRIMARY KEY (owner_kind, owner_id, blob_id)
) STRICT, WITHOUT ROWID;
CREATE INDEX blob_refs_blob ON blob_refs(blob_id);
CREATE INDEX blob_refs_thread ON blob_refs(thread_id);

CREATE TABLE cleanup_jobs (
    id INTEGER PRIMARY KEY,
    kind TEXT NOT NULL,
    repo TEXT NOT NULL,
    target TEXT NOT NULL,
    attempts INTEGER NOT NULL DEFAULT 0,
    last_error TEXT,
    created_at INTEGER NOT NULL
) STRICT;

CREATE INDEX items_thread ON items(thread_id);
CREATE INDEX operations_finished ON operations(finished_at) WHERE finished_at IS NOT NULL;
"#;

/// v4: background tasks (design.md §5.6). A task is stored as its protocol form (`task`, JSON)
/// with the columns queries select on. Items name the task they launched, interactions the
/// task that asked; an interaction that belongs to no turn keeps the turn it was asked during
/// (`anchor_turn_id`, internal: `thread/read` returns it with that turn). Turns keep what made
/// the agent start them (`start_trigger`).
const SCHEMA_V4: &str = r#"
CREATE TABLE background_tasks (
    id TEXT PRIMARY KEY NOT NULL,
    thread_id TEXT NOT NULL,
    status TEXT NOT NULL,
    ambient INTEGER NOT NULL DEFAULT 0,
    turn_id TEXT,
    started_at INTEGER NOT NULL,
    ended_at INTEGER,
    task TEXT NOT NULL
) STRICT;
CREATE INDEX background_tasks_thread ON background_tasks(thread_id, started_at);
CREATE INDEX background_tasks_turn ON background_tasks(turn_id) WHERE turn_id IS NOT NULL;
CREATE INDEX background_tasks_running ON background_tasks(status) WHERE status = 'running';

ALTER TABLE items ADD COLUMN background_task_id TEXT;
ALTER TABLE interactions ADD COLUMN background_task_id TEXT;
ALTER TABLE interactions ADD COLUMN anchor_turn_id TEXT;
CREATE INDEX interactions_turn ON interactions(turn_id) WHERE turn_id IS NOT NULL;
CREATE INDEX interactions_anchor ON interactions(anchor_turn_id) WHERE anchor_turn_id IS NOT NULL;
ALTER TABLE turns ADD COLUMN start_trigger TEXT;
"#;

/// v5: the last end a thread's background work reached (`Thread.background.lastEnded`), one
/// row per thread (`ended`: the protocol form, JSON), kept apart from the tasks because a task
/// that starts a new run clears its own `ended_at` and the summary must not go back to an older
/// end then (`store::note_background_end`). Filled from the tasks stored so far.
const SCHEMA_V5: &str = r#"
CREATE TABLE background_last_ended (
    thread_id TEXT PRIMARY KEY NOT NULL,
    ended_at INTEGER NOT NULL,
    task_id TEXT NOT NULL,
    ended TEXT NOT NULL
) STRICT;
"#;

/// v6: what the harnesses' extended features need (design.md §5.5, §9.6). Threads keep their
/// plan and fast modes (`modes`, JSON; `NULL`: both off) and the harness's last word on fast
/// mode (`fast_mode_state`), where a fork at a turn branches its source (`fork_at`, JSON, while
/// `fork_source` waits for the first process), and whether the user's title still has to reach
/// the native session (`native_rename_pending`). Turns keep the harness's anchor to fork at
/// (`native_anchor`, JSON). Items say whether their work can be moved to the background now
/// (`backgroundable`). Projects keep the user's trust decisions per harness (`harness_trust`,
/// JSON; `NULL`: none). Existing rows get none of these.
const SCHEMA_V6: &str = r#"
ALTER TABLE threads ADD COLUMN modes TEXT;
ALTER TABLE threads ADD COLUMN fast_mode_state TEXT;
ALTER TABLE threads ADD COLUMN fork_at TEXT;
ALTER TABLE threads ADD COLUMN native_rename_pending INTEGER NOT NULL DEFAULT 0;
ALTER TABLE turns ADD COLUMN native_anchor TEXT;
ALTER TABLE items ADD COLUMN backgroundable INTEGER NOT NULL DEFAULT 0;
ALTER TABLE projects ADD COLUMN harness_trust TEXT;
"#;

/// v7: where a turn's anchor holds (design.md §9.6). Turns keep the native session their anchor
/// belongs to once the thread has moved to another one (`anchor_session`; `NULL`: the thread's
/// current session), and whether their input reached the agent (`delivered`; a turn the agent
/// started itself did). Rows written before are taken as delivered, except those whose error is
/// one the engine gives only to turns it never sent: a failed start (`spawnFailed`,
/// `resumeFailed`, `harnessUnavailable`), a fork refused before its start (`forkOutdated`) and
/// an interrupt before the agent started (`interrupted`).
const SCHEMA_V7: &str = r#"
ALTER TABLE turns ADD COLUMN anchor_session TEXT;
ALTER TABLE turns ADD COLUMN delivered INTEGER NOT NULL DEFAULT 1;
UPDATE turns SET delivered = 0
    WHERE error IS NOT NULL
      AND json_extract(error, '$.kind') IN ('spawnFailed', 'resumeFailed', 'harnessUnavailable', 'forkOutdated', 'interrupted');
"#;

// v8 has no statements of its own: the event log's item index also holds the event type
// (`events_item_type`, see `aas_eventlog::migrate`), so that delta compaction checks an item's
// completion with one lookup instead of reading the events it would have to check (design.md
// §6.1). Building it reads the whole log once.

/// Connection settings (from [`crate::Policy`]).
#[derive(Debug, Clone, Copy)]
pub struct DbOptions {
    /// `busy_timeout` of every connection.
    pub busy_timeout: Duration,
    /// `journal_size_limit` (bytes the WAL file is truncated to after a checkpoint).
    pub journal_size_limit: u64,
}

/// Test-only failpoint that makes write transactions fail as a broken disk would
/// (`SQLITE_IOERR`). It does not exist outside `cfg(test)` builds.
#[cfg(test)]
#[derive(Debug, Default)]
pub struct WriteFailpoint {
    /// Writes still to fail; `u64::MAX` fails every write until disarmed.
    remaining: std::sync::atomic::AtomicU64,
    /// Failures injected so far.
    injected: std::sync::atomic::AtomicU64,
}

#[cfg(test)]
impl WriteFailpoint {
    /// Fails the next `n` write transactions.
    pub fn fail_next(&self, n: u64) {
        self.remaining.store(n, std::sync::atomic::Ordering::SeqCst);
    }

    /// Fails every write transaction until [`disarm`](Self::disarm).
    pub fn fail_always(&self) {
        self.remaining
            .store(u64::MAX, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn disarm(&self) {
        self.remaining.store(0, std::sync::atomic::Ordering::SeqCst);
    }

    /// Failures injected so far.
    pub fn injected(&self) -> u64 {
        self.injected.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn trip(&self) -> CoreResult<()> {
        use std::sync::atomic::Ordering;
        let tripped = self
            .remaining
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| match n {
                0 => None,
                u64::MAX => Some(u64::MAX),
                n => Some(n - 1),
            })
            .is_ok();
        if !tripped {
            return Ok(());
        }
        self.injected.fetch_add(1, Ordering::SeqCst);
        Err(CoreError::Db(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_IOERR),
            Some("disk I/O error (injected by a test failpoint)".into()),
        )))
    }
}

/// The write transaction in progress: where it was started and since when. The liveness
/// contract with the watchdog reports a daemon whose writer stays held as not live (design.md
/// §18.2), and says which transaction holds it.
#[derive(Debug, Clone, Copy)]
pub struct WriteInProgress {
    /// The code that asked for the transaction ([`Db::write`], [`Db::write_blocking`], or the
    /// caller of `Shared::tx` and its variants).
    pub caller: &'static Location<'static>,
    pub since: Instant,
}

/// Records a write transaction as in progress for as long as it lives (it is created once the
/// writer is held and dropped before the writer is released).
struct InProgress<'a> {
    inner: &'a Inner,
    write: WriteInProgress,
}

impl<'a> InProgress<'a> {
    fn begin(inner: &'a Inner, caller: &'static Location<'static>) -> Self {
        let write = WriteInProgress {
            caller,
            since: Instant::now(),
        };
        *inner.in_progress.lock() = Some(write);
        Self { inner, write }
    }
}

impl Drop for InProgress<'_> {
    fn drop(&mut self) {
        *self.inner.in_progress.lock() = None;
        let held = self.write.since.elapsed();
        let limit = self.inner.options.busy_timeout;
        if held > limit {
            // Every other write waited this long: longer than a writer of another process
            // would have waited for the database before giving up.
            tracing::warn!(caller = %self.write.caller, ?held, ?limit, "a write transaction held the database writer longer than policy.sqlite_busy_timeout");
        }
    }
}

struct Inner {
    path: PathBuf,
    options: DbOptions,
    /// `None` once the database is closed.
    writer: Mutex<Option<Connection>>,
    /// The transaction that holds `writer` (see [`WriteInProgress`]).
    in_progress: Mutex<Option<WriteInProgress>>,
    readers: Mutex<Readers>,
    /// Signalled whenever a reader connection is handed back.
    returned: Condvar,
    #[cfg(test)]
    failpoint: WriteFailpoint,
}

/// The reader pool: idle connections and how many are in use.
#[derive(Default)]
struct Readers {
    idle: Vec<Connection>,
    in_use: usize,
    closed: bool,
}

/// A pooled reader connection, handed back (or closed, after [`Db::close`]) when dropped.
struct PooledReader {
    inner: Arc<Inner>,
    conn: Option<Connection>,
}

impl Drop for PooledReader {
    fn drop(&mut self) {
        let mut readers = self.inner.readers.lock();
        readers.in_use -= 1;
        if let Some(conn) = self.conn.take()
            && !readers.closed
        {
            readers.idle.push(conn);
        }
        self.inner.returned.notify_all();
    }
}

fn closed_error() -> CoreError {
    CoreError::Closed
}

/// Database handle (cheap to clone).
#[derive(Clone)]
pub struct Db {
    inner: Arc<Inner>,
}

fn open_connection(path: &Path, options: &DbOptions) -> rusqlite::Result<Connection> {
    let conn = Connection::open(path)?;
    conn.busy_timeout(options.busy_timeout)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    // FULL: a commit is durable before the daemon answers the client, which the
    // idempotency guarantees rely on.
    conn.pragma_update(None, "synchronous", "FULL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(
        None,
        "journal_size_limit",
        i64::try_from(options.journal_size_limit).unwrap_or(i64::MAX),
    )?;
    Ok(conn)
}

impl Db {
    /// Opens (creating if needed) and migrates the database.
    pub fn open(path: &Path, options: DbOptions) -> CoreResult<Db> {
        let mut conn = open_connection(path, &options)?;
        migrate(&mut conn)?;
        Ok(Db::with_writer(path.to_path_buf(), options, conn))
    }

    fn with_writer(path: PathBuf, options: DbOptions, writer: Connection) -> Db {
        Db {
            inner: Arc::new(Inner {
                path,
                options,
                writer: Mutex::new(Some(writer)),
                in_progress: Mutex::new(None),
                readers: Mutex::new(Readers::default()),
                returned: Condvar::new(),
                #[cfg(test)]
                failpoint: WriteFailpoint::default(),
            }),
        }
    }

    /// The write failpoint of this database (tests).
    #[cfg(test)]
    pub fn failpoint(&self) -> &WriteFailpoint {
        &self.inner.failpoint
    }

    /// In-memory database (tests). Readers share the writer's cache through a named URI.
    #[cfg(test)]
    pub fn open_in_memory() -> CoreResult<Db> {
        let name = format!(
            "file:aas-mem-{}?mode=memory&cache=shared",
            ulid::Ulid::generate()
        );
        let path = PathBuf::from(name);
        let mut conn = Connection::open_with_flags(
            &path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
                | rusqlite::OpenFlags::SQLITE_OPEN_CREATE
                | rusqlite::OpenFlags::SQLITE_OPEN_URI,
        )?;
        migrate(&mut conn)?;
        let options = DbOptions {
            busy_timeout: Duration::from_secs(5),
            journal_size_limit: 64 * 1024 * 1024,
        };
        Ok(Db::with_writer(path, options, conn))
    }

    fn reader(&self) -> CoreResult<PooledReader> {
        let idle = {
            let mut readers = self.inner.readers.lock();
            if readers.closed {
                return Err(closed_error());
            }
            readers.in_use += 1;
            readers.idle.pop()
        };
        // From here on the guard accounts for the connection (also when opening one fails).
        let mut pooled = PooledReader {
            inner: self.inner.clone(),
            conn: idle,
        };
        if pooled.conn.is_none() {
            let path = &self.inner.path;
            pooled.conn = Some(if path.to_string_lossy().starts_with("file:") {
                Connection::open_with_flags(
                    path,
                    rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
                        | rusqlite::OpenFlags::SQLITE_OPEN_URI,
                )?
            } else {
                open_connection(path, &self.inner.options)?
            });
        }
        Ok(pooled)
    }

    /// Closes every connection: waits for reads and the write in progress, then closes the
    /// files (checkpointing the WAL). Later calls fail with an internal error. Used when the
    /// engine stops for good, so that the database files are free again (e.g. to delete them).
    pub async fn close(&self) -> CoreResult<()> {
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || {
            {
                let mut readers = inner.readers.lock();
                readers.closed = true;
                readers.idle.clear();
                while readers.in_use > 0 {
                    inner.returned.wait(&mut readers);
                }
            }
            let writer = inner.writer.lock().take();
            if let Some(conn) = writer {
                conn.close().map_err(|(_, e)| CoreError::Db(e))?;
            }
            Ok(())
        })
        .await
        .map_err(|e| CoreError::Internal(format!("database task failed: {e}")))?
    }

    /// Runs `f` in an immediate write transaction and commits it.
    #[track_caller]
    pub fn write<T, F>(&self, f: F) -> impl Future<Output = CoreResult<T>> + Send + 'static
    where
        T: Send + 'static,
        F: FnOnce(&Transaction<'_>) -> CoreResult<T> + Send + 'static,
    {
        self.clone().write_at(Location::caller(), f)
    }

    /// [`write`](Self::write) on behalf of `caller` (for wrappers that pass their own caller
    /// on, so that [`write_in_progress`](Self::write_in_progress) names the code that wrote).
    pub(crate) async fn write_at<T, F>(
        self,
        caller: &'static Location<'static>,
        f: F,
    ) -> CoreResult<T>
    where
        T: Send + 'static,
        F: FnOnce(&Transaction<'_>) -> CoreResult<T> + Send + 'static,
    {
        tokio::task::spawn_blocking(move || self.write_from(caller, f))
            .await
            .map_err(|e| CoreError::Internal(format!("database task failed: {e}")))?
    }

    /// Runs `f` in a read transaction (a consistent snapshot).
    pub async fn read<T, F>(&self, f: F) -> CoreResult<T>
    where
        T: Send + 'static,
        F: FnOnce(&Transaction<'_>) -> CoreResult<T> + Send + 'static,
    {
        let db = self.clone();
        tokio::task::spawn_blocking(move || {
            let mut pooled = db.reader()?;
            let conn = pooled
                .conn
                .as_mut()
                .expect("a pooled reader holds its connection");
            let tx = conn.transaction_with_behavior(TransactionBehavior::Deferred)?;
            let value = f(&tx);
            tx.finish()?;
            value
        })
        .await
        .map_err(|e| CoreError::Internal(format!("database task failed: {e}")))?
    }

    /// Synchronous write (startup code that is not on the runtime yet, and work that runs on
    /// a blocking thread anyway).
    #[track_caller]
    pub fn write_blocking<T>(
        &self,
        f: impl FnOnce(&Transaction<'_>) -> CoreResult<T>,
    ) -> CoreResult<T> {
        self.write_from(Location::caller(), f)
    }

    fn write_from<T>(
        &self,
        caller: &'static Location<'static>,
        f: impl FnOnce(&Transaction<'_>) -> CoreResult<T>,
    ) -> CoreResult<T> {
        let mut writer = self.inner.writer.lock();
        let conn = writer.as_mut().ok_or_else(closed_error)?;
        // Declared after the writer's guard and before the transaction: it ends after the
        // transaction (committed or rolled back) and before the writer is released.
        let _in_progress = InProgress::begin(&self.inner, caller);
        #[cfg(test)]
        self.inner.failpoint.trip()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let value = f(&tx)?;
        tx.commit()?;
        Ok(value)
    }

    /// The write transaction in progress, if any.
    pub fn write_in_progress(&self) -> Option<WriteInProgress> {
        *self.inner.in_progress.lock()
    }

    /// Runs `PRAGMA quick_check` (used by `doctor`).
    pub async fn quick_check(&self) -> CoreResult<String> {
        self.read(|tx| Ok(tx.query_row("PRAGMA quick_check", [], |r| r.get::<_, String>(0))?))
            .await
    }

    /// Returns the database's free pages to the file system, at most `pages_per_tx` in one
    /// write transaction (so agent events never wait long), until none is left. Returns the
    /// number of pages returned.
    pub async fn incremental_vacuum(&self, pages_per_tx: u32) -> CoreResult<u64> {
        let mut total = 0u64;
        loop {
            let freed = self
                .write(move |tx| {
                    let before: i64 = tx.query_row("PRAGMA freelist_count", [], |r| r.get(0))?;
                    if before == 0 {
                        return Ok(0);
                    }
                    tx.execute_batch(&format!("PRAGMA incremental_vacuum({pages_per_tx})"))?;
                    let after: i64 = tx.query_row("PRAGMA freelist_count", [], |r| r.get(0))?;
                    Ok(u64::try_from(before - after).unwrap_or(0))
                })
                .await?;
            if freed == 0 {
                return Ok(total);
            }
            total += freed;
        }
    }

    /// Pages of the database file and how many of them are free.
    pub async fn page_counts(&self) -> CoreResult<(u64, u64)> {
        self.read(|tx| {
            let pages: i64 = tx.query_row("PRAGMA page_count", [], |r| r.get(0))?;
            let free: i64 = tx.query_row("PRAGMA freelist_count", [], |r| r.get(0))?;
            Ok((pages as u64, free as u64))
        })
        .await
    }
}

fn migrate(conn: &mut Connection) -> CoreResult<()> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version > SCHEMA_VERSION {
        return Err(CoreError::Internal(format!(
            "database schema version {version} is newer than this build ({SCHEMA_VERSION}); refusing to open"
        )));
    }
    if version < 1 {
        // Possible only before the first table exists (older files are rebuilt below).
        conn.pragma_update(None, "auto_vacuum", "INCREMENTAL")?;
        let tx = conn.transaction()?;
        tx.execute_batch(SCHEMA_V1)?;
        aas_eventlog::migrate(&tx)?;
        tx.pragma_update(None, "user_version", 1)?;
        tx.commit()?;
    }
    if version < 2 {
        let tx = conn.transaction()?;
        tx.execute_batch(SCHEMA_V2)?;
        tx.pragma_update(None, "user_version", 2)?;
        tx.commit()?;
    }
    if version < 3 {
        let tx = conn.transaction()?;
        tx.execute_batch(SCHEMA_V3)?;
        aas_eventlog::migrate(&tx)?;
        crate::store::backfill_blob_refs(&tx, crate::store::now_ms())?;
        tx.pragma_update(None, "user_version", 3)?;
        tx.commit()?;
    }
    if version < 4 {
        let tx = conn.transaction()?;
        tx.execute_batch(SCHEMA_V4)?;
        tx.pragma_update(None, "user_version", 4)?;
        tx.commit()?;
    }
    if version < 5 {
        let tx = conn.transaction()?;
        tx.execute_batch(SCHEMA_V5)?;
        crate::store::backfill_background_last_ended(&tx)?;
        tx.pragma_update(None, "user_version", 5)?;
        tx.commit()?;
    }
    if version < 6 {
        let tx = conn.transaction()?;
        tx.execute_batch(SCHEMA_V6)?;
        tx.pragma_update(None, "user_version", 6)?;
        tx.commit()?;
    }
    if version < 7 {
        let tx = conn.transaction()?;
        tx.execute_batch(SCHEMA_V7)?;
        tx.pragma_update(None, "user_version", 7)?;
        tx.commit()?;
    }
    if version < 8 {
        let tx = conn.transaction()?;
        aas_eventlog::migrate(&tx)?;
        tx.pragma_update(None, "user_version", 8)?;
        tx.commit()?;
    }
    // A file created before incremental vacuuming was enabled is rebuilt with it once.
    let auto_vacuum: i64 = conn.query_row("PRAGMA auto_vacuum", [], |r| r.get(0))?;
    if auto_vacuum != AUTO_VACUUM_INCREMENTAL {
        conn.pragma_update(None, "auto_vacuum", "INCREMENTAL")?;
        conn.execute_batch("VACUUM")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> DbOptions {
        DbOptions {
            busy_timeout: Duration::from_secs(5),
            journal_size_limit: 1024 * 1024,
        }
    }

    fn pragma(conn: &Connection, name: &str) -> i64 {
        conn.query_row(&format!("PRAGMA {name}"), [], |r| r.get(0))
            .unwrap()
    }

    #[tokio::test]
    async fn open_migrate_read_write() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("t.db"), opts()).unwrap();
        db.write(|tx| {
            tx.execute("INSERT INTO meta (key, value) VALUES ('a', 'b')", [])?;
            Ok(())
        })
        .await
        .unwrap();
        let v: String = db
            .read(|tx| Ok(tx.query_row("SELECT value FROM meta WHERE key='a'", [], |r| r.get(0))?))
            .await
            .unwrap();
        assert_eq!(v, "b");
        assert_eq!(db.quick_check().await.unwrap(), "ok");
        {
            let writer = db.inner.writer.lock();
            let conn = writer.as_ref().unwrap();
            assert_eq!(
                pragma(conn, "auto_vacuum"),
                AUTO_VACUUM_INCREMENTAL,
                "a new file vacuums incrementally"
            );
            assert_eq!(pragma(conn, "journal_size_limit"), 1024 * 1024);
            assert_eq!(pragma(conn, "busy_timeout"), 5000);
        }
        drop(db);
        // Reopening keeps the data and does not re-run migrations.
        let db = Db::open(&dir.path().join("t.db"), opts()).unwrap();
        let n: i64 = db
            .read(|tx| Ok(tx.query_row("SELECT count(*) FROM meta", [], |r| r.get(0))?))
            .await
            .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn a_version_1_database_is_migrated_in_place() {
        use aas_protocol::{Attachment, BlobId, ItemBody, UserMessageDelivery};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v1.db");
        let attached = BlobId::from_sha256_hex(&"a".repeat(64));
        let orphan = BlobId::from_sha256_hex(&"b".repeat(64));
        {
            // A file of the first layout, created without incremental vacuuming.
            let mut conn = open_connection(&path, &opts()).unwrap();
            let tx = conn.transaction().unwrap();
            tx.execute_batch(SCHEMA_V1).unwrap();
            tx.execute_batch(
                "CREATE TABLE streams (name TEXT PRIMARY KEY NOT NULL, head INTEGER NOT NULL) STRICT;
                 CREATE TABLE events (stream TEXT NOT NULL, seq INTEGER NOT NULL, ts INTEGER NOT NULL, type TEXT NOT NULL,
                                      data TEXT NOT NULL, item_id TEXT, PRIMARY KEY (stream, seq)) STRICT, WITHOUT ROWID;",
            )
            .unwrap();
            tx.pragma_update(None, "user_version", 1).unwrap();
            tx.execute(
                "INSERT INTO operations (id, kind, status, started_at) VALUES ('op_1', 'gitClone', 'failed', 1)",
                [],
            )
            .unwrap();
            let body = ItemBody::UserMessage {
                text: "look".into(),
                attachments: vec![Attachment::Image {
                    blob_id: attached.clone(),
                    mime: "image/png".into(),
                }],
                mentions: Vec::new(),
                delivery: UserMessageDelivery::Normal,
            };
            tx.execute(
                "INSERT INTO items (id, thread_id, turn_id, ord, status, started_at, body) VALUES ('itm_1', 'thr_1', 'trn_1', 1, 'completed', 1, ?1)",
                [serde_json::to_string(&body).unwrap()],
            )
            .unwrap();
            for id in [&attached, &orphan] {
                tx.execute(
                    "INSERT INTO blobs (id, mime, size, created_at) VALUES (?1, 'image/png', 1, 1)",
                    [id.as_str()],
                )
                .unwrap();
            }
            tx.commit().unwrap();
            assert_eq!(pragma(&conn, "auto_vacuum"), 0);
        }
        let db = Db::open(&path, opts()).unwrap();
        let writer = db.inner.writer.lock();
        let conn = writer.as_ref().unwrap();
        assert_eq!(pragma(conn, "user_version"), SCHEMA_VERSION);
        assert_eq!(
            pragma(conn, "auto_vacuum"),
            AUTO_VACUUM_INCREMENTAL,
            "the file was rebuilt with incremental vacuuming"
        );
        let (progress, work_dir): (Option<String>, Option<String>) = conn
            .query_row(
                "SELECT progress, work_dir FROM operations WHERE id = 'op_1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            (progress, work_dir),
            (None, None),
            "existing rows keep working"
        );
        let refs: Vec<(String, String, String)> = conn
            .prepare("SELECT blob_id, owner_kind, owner_id FROM blob_refs")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(
            refs,
            vec![(attached.to_string(), "item".to_owned(), "itm_1".to_owned())],
            "references are recorded from the items"
        );
        let orphaned = |id: &BlobId| -> Option<i64> {
            conn.query_row(
                "SELECT orphaned_at FROM blobs WHERE id = ?1",
                [id.as_str()],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(
            orphaned(&attached),
            None,
            "a referenced blob is not orphaned"
        );
        assert!(
            orphaned(&orphan).is_some(),
            "a blob nothing refers to starts its grace period"
        );
    }

    /// A version 3 database gains the background tasks (v4); what it held reads as before, with
    /// nothing linked to a background task and no turn trigger.
    #[tokio::test]
    async fn a_version_3_database_gains_background_tasks() {
        use aas_protocol::{
            BackgroundTaskStatus, InteractionId, ItemId, ThreadId, TurnId, examples,
        };
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v3.db");
        {
            let mut conn = open_connection(&path, &opts()).unwrap();
            let tx = conn.transaction().unwrap();
            tx.execute_batch(SCHEMA_V1).unwrap();
            aas_eventlog::migrate(&tx).unwrap();
            tx.execute_batch(SCHEMA_V2).unwrap();
            tx.execute_batch(SCHEMA_V3).unwrap();
            tx.pragma_update(None, "user_version", 3).unwrap();
            tx.execute(
                "INSERT INTO turns (id, thread_id, idx, status, started_at) VALUES ('trn_1', 'thr_1', 0, 'completed', 1)",
                [],
            )
            .unwrap();
            let body =
                serde_json::to_string(&aas_protocol::ItemBody::AgentMessage { text: "hi".into() })
                    .unwrap();
            tx.execute(
                "INSERT INTO items (id, thread_id, turn_id, ord, status, started_at, body) VALUES ('itm_1', 'thr_1', 'trn_1', 1, 'completed', 1, ?1)",
                [body],
            )
            .unwrap();
            let request = serde_json::to_string(&examples::approval().request).unwrap();
            tx.execute(
                "INSERT INTO interactions (id, thread_id, turn_id, status, created_at, request, adapter_request_id)
                 VALUES ('int_1', 'thr_1', 'trn_1', 'pending', 1, ?1, 'r1')",
                [request],
            )
            .unwrap();
            tx.commit().unwrap();
        }
        let db = Db::open(&path, opts()).unwrap();
        db.write(|tx| {
            let version: i64 = tx.query_row("PRAGMA user_version", [], |r| r.get(0))?;
            assert_eq!(version, SCHEMA_VERSION);
            let turn = crate::store::get_turn(tx, &TurnId::from("trn_1"))?.unwrap();
            assert_eq!(turn.turn.trigger, None);
            let items = crate::store::items_of_turns(tx, &[TurnId::from("trn_1")])?;
            assert_eq!(items[0].id, ItemId::from("itm_1"));
            assert_eq!(items[0].background_task_id, None);
            let interaction =
                crate::store::get_interaction(tx, &InteractionId::from("int_1"))?.unwrap();
            assert_eq!(interaction.interaction.background_task_id, None);
            assert_eq!(interaction.anchor_turn_id, None);
            // The new table takes tasks and answers the thread's summary.
            let task = aas_protocol::BackgroundTask {
                thread_id: ThreadId::from("thr_1"),
                ..examples::background_task()
            };
            crate::store::upsert_background_task(tx, &task)?;
            let ended = aas_protocol::BackgroundTask {
                status: BackgroundTaskStatus::Completed,
                ended_at: Some(5),
                ..task.clone()
            };
            crate::store::upsert_background_task(tx, &ended)?;
            assert_eq!(
                crate::store::get_background_task(tx, &task.id)?,
                Some(ended.clone())
            );
            let summary = crate::store::thread_background(tx, &ThreadId::from("thr_1"))?;
            assert_eq!(summary.running, 0);
            assert_eq!(summary.last_ended.unwrap().task_id, task.id);
            Ok(())
        })
        .await
        .unwrap();
    }

    /// A background task with the given id, `ended_at` (`None`: running) and `ambient` flag.
    fn task_ended(n: u32, ended_at: Option<i64>, ambient: bool) -> aas_protocol::BackgroundTask {
        use aas_protocol::{BackgroundTaskStatus, ThreadId, examples};
        aas_protocol::BackgroundTask {
            id: examples::background_task_id(n),
            thread_id: ThreadId::from("thr_1"),
            native_id: format!("native-{n}"),
            title: format!("task {n}"),
            status: match ended_at {
                Some(_) => BackgroundTaskStatus::Completed,
                None => BackgroundTaskStatus::Running,
            },
            ambient,
            ended_at,
            ..examples::background_task()
        }
    }

    /// A version 4 database keeps the last end of each thread's background work apart from the
    /// tasks (v5), filled from what it held: the latest end that is not ambient.
    #[tokio::test]
    async fn a_version_4_database_gains_the_last_end_of_background_work() {
        use aas_protocol::ThreadId;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v4.db");
        {
            let mut conn = open_connection(&path, &opts()).unwrap();
            let tx = conn.transaction().unwrap();
            tx.execute_batch(SCHEMA_V1).unwrap();
            aas_eventlog::migrate(&tx).unwrap();
            tx.execute_batch(SCHEMA_V2).unwrap();
            tx.execute_batch(SCHEMA_V3).unwrap();
            tx.execute_batch(SCHEMA_V4).unwrap();
            tx.pragma_update(None, "user_version", 4).unwrap();
            for task in [
                task_ended(1, Some(5), false),
                task_ended(2, Some(7), false),
                task_ended(3, Some(9), true),
                task_ended(4, None, false),
            ] {
                tx.execute(
                    "INSERT INTO background_tasks (id, thread_id, status, ambient, started_at, ended_at, task)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    rusqlite::params![
                        task.id.as_str(),
                        task.thread_id.as_str(),
                        task.status.as_str(),
                        task.ambient,
                        task.started_at,
                        task.ended_at,
                        serde_json::to_string(&task).unwrap(),
                    ],
                )
                .unwrap();
            }
            tx.commit().unwrap();
        }
        let db = Db::open(&path, opts()).unwrap();
        db.write(|tx| {
            let version: i64 = tx.query_row("PRAGMA user_version", [], |r| r.get(0))?;
            assert_eq!(version, SCHEMA_VERSION);
            let summary = crate::store::thread_background(tx, &ThreadId::from("thr_1"))?;
            assert_eq!(summary.running, 1);
            let last = summary.last_ended.unwrap();
            assert_eq!(
                (last.task_id, last.ended_at),
                (task_ended(2, None, false).id, 7)
            );
            Ok(())
        })
        .await
        .unwrap();
    }

    /// A version 5 database gains what the extended harness features keep (v6): rows written
    /// before read back with no modes, no fork point, no pending rename, no turn anchor, no
    /// backgroundable item and no trust decision; the new columns round-trip.
    #[tokio::test]
    async fn a_version_5_database_gains_modes_anchors_and_trust() {
        use aas_protocol::{ThreadId, TurnId};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v5.db");
        {
            let mut conn = open_connection(&path, &opts()).unwrap();
            let tx = conn.transaction().unwrap();
            tx.execute_batch(SCHEMA_V1).unwrap();
            aas_eventlog::migrate(&tx).unwrap();
            for schema in [SCHEMA_V2, SCHEMA_V3, SCHEMA_V4, SCHEMA_V5] {
                tx.execute_batch(schema).unwrap();
            }
            tx.pragma_update(None, "user_version", 5).unwrap();
            tx.execute(
                "INSERT INTO threads (id, project_id, harness_id, title, title_source, cwd, workspace, settings, status,
                    usage, created_at, updated_at, last_activity_at)
                 VALUES ('thr_1', 'prj_1', 'fake', 't', 'user', 'C:\\x', '{\"kind\":\"local\"}', '{}', 'idle',
                    '{\"inputTokens\":0,\"outputTokens\":0,\"cachedInputTokens\":0,\"reasoningTokens\":0}', 1, 1, 1)",
                [],
            )
            .unwrap();
            tx.execute(
                "INSERT INTO turns (id, thread_id, idx, status, started_at) VALUES ('trn_1', 'thr_1', 0, 'completed', 1)",
                [],
            )
            .unwrap();
            tx.execute(
                "INSERT INTO projects (id, name, path, path_key, created_at, updated_at, defaults)
                 VALUES ('prj_1', 'p', 'C:\\x', 'c:/x', 1, 1, '{}')",
                [],
            )
            .unwrap();
            tx.commit().unwrap();
        }
        let db = Db::open(&path, opts()).unwrap();
        db.write(|tx| {
            let version: i64 = tx.query_row("PRAGMA user_version", [], |r| r.get(0))?;
            assert_eq!(version, SCHEMA_VERSION);
            let mut thread = crate::store::get_thread(tx, &ThreadId::from("thr_1"))?.unwrap();
            assert_eq!(thread.modes, aas_protocol::ThreadModes::default());
            assert_eq!(
                (
                    &thread.fast_mode_state,
                    &thread.fork_at,
                    thread.native_rename_pending
                ),
                (&None, &None, false)
            );
            let mut turn = crate::store::get_turn(tx, &TurnId::from("trn_1"))?.unwrap();
            assert_eq!(
                (turn.native_anchor.clone(), turn.turn.forkable),
                (None, false)
            );
            let project =
                crate::store::get_project(tx, &aas_protocol::ProjectId::from("prj_1"))?.unwrap();
            assert!(project.project.harness_trust.is_empty());

            thread.modes.plan = true;
            thread.fast_mode_state = Some("on".into());
            thread.fork_at = Some(aas_harness::ForkPoint {
                anchor: serde_json::json!({"turn": 3}),
                before: true,
                previous: Some(serde_json::json!({"turn": 2})),
            });
            thread.native_rename_pending = true;
            crate::store::update_thread(tx, &thread)?;
            assert_eq!(crate::store::get_thread(tx, &thread.id)?.unwrap(), thread);
            turn.native_anchor = Some(serde_json::json!("turn-id"));
            crate::store::update_turn(tx, &turn)?;
            let read = crate::store::get_turn(tx, &turn.turn.id)?.unwrap();
            assert!(read.turn.forkable);
            assert_eq!(read.native_anchor, turn.native_anchor);
            Ok(())
        })
        .await
        .unwrap();
    }

    /// A version 6 database gains where turn anchors hold (v7): turns the engine never sent
    /// (their error says so) are not delivered, every other turn is; no anchor names another
    /// native session. Replacing an anchor and moving to another session touch only the anchors
    /// of the thread's current session.
    #[tokio::test]
    async fn a_version_6_database_gains_anchor_sessions_and_delivery() {
        use aas_protocol::{ThreadId, TurnId};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v6.db");
        {
            let mut conn = open_connection(&path, &opts()).unwrap();
            let tx = conn.transaction().unwrap();
            tx.execute_batch(SCHEMA_V1).unwrap();
            aas_eventlog::migrate(&tx).unwrap();
            for schema in [SCHEMA_V2, SCHEMA_V3, SCHEMA_V4, SCHEMA_V5, SCHEMA_V6] {
                tx.execute_batch(schema).unwrap();
            }
            tx.pragma_update(None, "user_version", 6).unwrap();
            let rows = [
                ("trn_0", 0, "completed", None, Some("{\"turn\":0}")),
                ("trn_1", 1, "failed", Some("resumeFailed"), None),
                ("trn_2", 2, "interrupted", Some("interrupted"), None),
                ("trn_3", 3, "failed", Some("adapterError"), None),
                ("trn_4", 4, "interrupted", None, Some("{\"turn\":1}")),
                ("trn_5", 5, "failed", Some("spawnFailed"), None),
                ("trn_6", 6, "failed", Some("forkOutdated"), None),
                ("trn_7", 7, "failed", Some("harnessUnavailable"), None),
            ];
            for (id, idx, status, kind, anchor) in rows {
                let error = kind.map(|k| format!("{{\"message\":\"m\",\"kind\":\"{k}\"}}"));
                tx.execute(
                    "INSERT INTO turns (id, thread_id, idx, status, started_at, error, native_anchor)
                     VALUES (?1, 'thr_1', ?2, ?3, 1, ?4, ?5)",
                    rusqlite::params![id, idx, status, error, anchor],
                )
                .unwrap();
            }
            tx.commit().unwrap();
        }
        let db = Db::open(&path, opts()).unwrap();
        db.write(|tx| {
            let version: i64 = tx.query_row("PRAGMA user_version", [], |r| r.get(0))?;
            assert_eq!(version, SCHEMA_VERSION);
            let thread = ThreadId::from("thr_1");
            let turns = crate::store::all_turns(tx, &thread)?;
            assert_eq!(
                turns.iter().map(|t| t.delivered).collect::<Vec<_>>(),
                [true, false, false, true, true, false, false, false]
            );
            assert!(turns.iter().all(|t| t.anchor_session.is_none()));

            // A switch keeps the anchors so far with the earlier session; a later anchor of
            // the same value belongs to the current one and is the only one replaced.
            crate::store::keep_anchors_with_session(tx, &thread, "ses-1")?;
            let mut later = crate::store::get_turn(tx, &TurnId::from("trn_3"))?.unwrap();
            later.native_anchor = Some(serde_json::json!({"turn": 0}));
            crate::store::update_turn(tx, &later)?;
            assert!(crate::store::replace_turn_anchor(
                tx,
                &thread,
                &serde_json::json!({"turn": 0}),
                &serde_json::json!({"turn": 9}),
            )?);
            let anchors: Vec<_> = crate::store::all_turns(tx, &thread)?
                .into_iter()
                .map(|t| (t.native_anchor, t.anchor_session))
                .collect();
            assert_eq!(
                anchors[0],
                (Some(serde_json::json!({"turn": 0})), Some("ses-1".into()))
            );
            assert_eq!(anchors[3], (Some(serde_json::json!({"turn": 9})), None));
            assert_eq!(
                anchors[4],
                (Some(serde_json::json!({"turn": 1})), Some("ses-1".into()))
            );
            assert_eq!(anchors[1], (None, None));
            Ok(())
        })
        .await
        .unwrap();
    }

    /// A version 7 database gets the event log's item index that also holds the event type
    /// (v8); the item index it replaces is gone.
    #[tokio::test]
    async fn a_version_7_database_gains_the_item_type_index() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v7.db");
        {
            let mut conn = open_connection(&path, &opts()).unwrap();
            let tx = conn.transaction().unwrap();
            tx.execute_batch(SCHEMA_V1).unwrap();
            aas_eventlog::migrate(&tx).unwrap();
            // The item index as version 7 had it.
            tx.execute_batch(
                "DROP INDEX events_item_type;
                 CREATE INDEX events_item ON events(item_id) WHERE item_id IS NOT NULL;",
            )
            .unwrap();
            for schema in [
                SCHEMA_V2, SCHEMA_V3, SCHEMA_V4, SCHEMA_V5, SCHEMA_V6, SCHEMA_V7,
            ] {
                tx.execute_batch(schema).unwrap();
            }
            tx.pragma_update(None, "user_version", 7).unwrap();
            tx.commit().unwrap();
        }
        let db = Db::open(&path, opts()).unwrap();
        let (version, indexes) = db
            .read(|tx| {
                let version: i64 = tx.query_row("PRAGMA user_version", [], |r| r.get(0))?;
                let indexes = tx
                    .prepare(
                        "SELECT name FROM sqlite_master WHERE type = 'index' AND tbl_name = 'events'",
                    )?
                    .query_map([], |r| r.get::<_, String>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok((version, indexes))
            })
            .await
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        assert!(
            indexes.iter().any(|i| i == "events_item_type"),
            "{indexes:?}"
        );
        assert!(!indexes.iter().any(|i| i == "events_item"), "{indexes:?}");
    }

    /// `lastEnded` never goes back: a task that starts a new run under the same id keeps its
    /// previous end as the thread's last one until a later end comes; an older end written
    /// late does not replace a later one; the same end written again takes its new state.
    #[tokio::test]
    async fn the_last_end_of_background_work_only_moves_forward() {
        use aas_protocol::{BackgroundTaskStatus, ThreadId};
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("m.db"), opts()).unwrap();
        db.write(|tx| {
            let thread = ThreadId::from("thr_1");
            let last = |tx: &Transaction<'_>| {
                crate::store::thread_background(tx, &thread)
                    .map(|s| s.last_ended.map(|e| (e.task_id, e.ended_at, e.status)))
            };
            let (b, a) = (
                task_ended(1, Some(10), false),
                task_ended(2, Some(20), false),
            );
            assert_eq!(last(tx)?, None);
            crate::store::upsert_background_task(tx, &b)?;
            crate::store::upsert_background_task(tx, &a)?;
            let a_first = Some((a.id.clone(), 20, BackgroundTaskStatus::Completed));
            assert_eq!(last(tx)?, a_first);
            // A's second run: B (older) must not become the last end again.
            let a_again = aas_protocol::BackgroundTask {
                runs: 2,
                ..task_ended(2, None, false)
            };
            crate::store::upsert_background_task(tx, &a_again)?;
            assert_eq!(last(tx)?, a_first);
            assert_eq!(
                crate::store::thread_background(tx, &thread)?.running,
                1,
                "the running count follows the task"
            );
            // An end that is older than the last one, written late, changes nothing.
            crate::store::upsert_background_task(tx, &task_ended(3, Some(15), false))?;
            assert_eq!(last(tx)?, a_first);
            // Ambient work never becomes the last end.
            crate::store::upsert_background_task(tx, &task_ended(4, Some(40), true))?;
            assert_eq!(last(tx)?, a_first);
            // The second run's end is the new last end; the same end written again takes its
            // new status.
            let a_second = aas_protocol::BackgroundTask {
                runs: 2,
                ..task_ended(2, Some(30), false)
            };
            crate::store::upsert_background_task(tx, &a_second)?;
            assert_eq!(
                last(tx)?,
                Some((a.id.clone(), 30, BackgroundTaskStatus::Completed))
            );
            let a_stopped = aas_protocol::BackgroundTask {
                status: BackgroundTaskStatus::Stopped,
                ..a_second
            };
            crate::store::upsert_background_task(tx, &a_stopped)?;
            assert_eq!(
                last(tx)?,
                Some((a.id.clone(), 30, BackgroundTaskStatus::Stopped))
            );
            // Purging the thread removes it.
            crate::store::purge_thread(tx, &thread, 100)?;
            assert_eq!(last(tx)?, None);
            Ok(())
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn incremental_vacuum_returns_free_pages() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("v.db"), opts()).unwrap();
        db.write(|tx| {
            for i in 0..400 {
                tx.execute(
                    "INSERT INTO meta (key, value) VALUES (?1, ?2)",
                    rusqlite::params![format!("k{i}"), "x".repeat(4000)],
                )?;
            }
            Ok(())
        })
        .await
        .unwrap();
        let (pages_before, _) = db.page_counts().await.unwrap();
        db.write(|tx| {
            tx.execute("DELETE FROM meta", [])?;
            Ok(())
        })
        .await
        .unwrap();
        let (_, free) = db.page_counts().await.unwrap();
        assert!(free > 100, "deleting leaves free pages ({free})");
        let returned = db.incremental_vacuum(64).await.unwrap();
        assert_eq!(
            returned, free,
            "every free page is returned, 64 per transaction"
        );
        let (pages_after, free_after) = db.page_counts().await.unwrap();
        assert_eq!(free_after, 0);
        assert!(
            pages_after + free <= pages_before,
            "the file shrank: {pages_before} -> {pages_after}"
        );
        assert_eq!(
            db.incremental_vacuum(64).await.unwrap(),
            0,
            "nothing left to return"
        );
    }

    #[tokio::test]
    async fn the_failpoint_fails_writes_until_disarmed() {
        let db = Db::open_in_memory().unwrap();
        db.failpoint().fail_next(2);
        for _ in 0..2 {
            let err = db.write(|_| Ok(())).await.unwrap_err();
            assert!(err.is_storage_failure(), "{err}");
        }
        db.write(|_| Ok(())).await.unwrap();
        db.failpoint().fail_always();
        for _ in 0..3 {
            assert!(db.write(|_| Ok(())).await.is_err());
        }
        assert!(db.read(|_| Ok(())).await.is_ok(), "reads are not affected");
        db.failpoint().disarm();
        db.write(|_| Ok(())).await.unwrap();
        assert_eq!(db.failpoint().injected(), 5);
    }

    #[tokio::test]
    async fn a_closed_database_releases_its_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("closing.db");
        let db = Db::open(&path, opts()).unwrap();
        db.write(|tx| {
            tx.execute("INSERT INTO meta (key, value) VALUES ('k', 'v')", [])?;
            Ok(())
        })
        .await
        .unwrap();
        let n: i64 = db
            .read(|tx| Ok(tx.query_row("SELECT count(*) FROM meta", [], |r| r.get(0))?))
            .await
            .unwrap();
        assert_eq!(n, 1);
        db.close().await.unwrap();
        assert!(
            matches!(db.read(|_| Ok(())).await, Err(CoreError::Closed)),
            "reads fail once closed"
        );
        let write = db.write(|_| Ok(())).await.unwrap_err();
        assert!(
            matches!(write, CoreError::Closed) && !write.is_storage_failure(),
            "writes fail once closed, and not as a storage failure"
        );
        for file in ["closing.db", "closing.db-wal", "closing.db-shm"] {
            let f = dir.path().join(file);
            if f.exists() {
                std::fs::remove_file(&f).unwrap_or_else(|e| panic!("{file} is still open: {e}"));
            }
        }
    }

    #[tokio::test]
    async fn in_memory_readers_see_writes() {
        let db = Db::open_in_memory().unwrap();
        db.write(|tx| {
            tx.execute("INSERT INTO meta (key, value) VALUES ('x', 'y')", [])?;
            Ok(())
        })
        .await
        .unwrap();
        let v: String = db
            .read(|tx| Ok(tx.query_row("SELECT value FROM meta WHERE key='x'", [], |r| r.get(0))?))
            .await
            .unwrap();
        assert_eq!(v, "y");
    }
}
