//! SQLite access: one writer connection (serialized), a pool of readers, WAL mode.
//! All calls run on the blocking pool.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::{Condvar, Mutex};
use rusqlite::{Connection, Transaction, TransactionBehavior};

use crate::error::{CoreError, CoreResult};

const SCHEMA_VERSION: i64 = 3;

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

struct Inner {
    path: PathBuf,
    options: DbOptions,
    /// `None` once the database is closed.
    writer: Mutex<Option<Connection>>,
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
    pub async fn write<T, F>(&self, f: F) -> CoreResult<T>
    where
        T: Send + 'static,
        F: FnOnce(&Transaction<'_>) -> CoreResult<T> + Send + 'static,
    {
        let db = self.clone();
        tokio::task::spawn_blocking(move || db.write_blocking(f))
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
    pub fn write_blocking<T>(
        &self,
        f: impl FnOnce(&Transaction<'_>) -> CoreResult<T>,
    ) -> CoreResult<T> {
        let mut writer = self.inner.writer.lock();
        let conn = writer.as_mut().ok_or_else(closed_error)?;
        #[cfg(test)]
        self.inner.failpoint.trip()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let value = f(&tx)?;
        tx.commit()?;
        Ok(value)
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
