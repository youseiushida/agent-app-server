//! Row mapping and queries. Every function takes a connection or transaction; callers own
//! transaction boundaries.

use aas_protocol::methods::ThreadCursor;
use aas_protocol::*;
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::error::{CoreError, CoreResult};

pub fn now_ms() -> Millis {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn to_json<T: Serialize>(v: &T) -> String {
    serde_json::to_string(v).expect("domain values serialize")
}

fn from_json<T: DeserializeOwned>(s: &str) -> CoreResult<T> {
    serde_json::from_str(s).map_err(|e| CoreError::Corrupt(format!("{e}: {s}")))
}

fn opt_json<T: DeserializeOwned>(s: Option<String>) -> CoreResult<Option<T>> {
    s.map(|s| from_json(&s)).transpose()
}

fn b(v: i64) -> bool {
    v != 0
}

// ----- meta -------------------------------------------------------------------------------------

pub fn meta_get(conn: &Connection, key: &str) -> CoreResult<Option<String>> {
    Ok(conn
        .query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
        .optional()?)
}

/// `meta` key: when Windows ended the session that stopped the daemon (written by
/// `Engine::shutdown_for_end_session`, read and removed by the next start's recovery).
pub const META_SESSION_ENDED_AT: &str = "session_ended_at";

pub fn meta_delete(conn: &Connection, key: &str) -> CoreResult<()> {
    conn.execute("DELETE FROM meta WHERE key = ?1", [key])?;
    Ok(())
}

pub fn meta_set(conn: &Connection, key: &str, value: &str) -> CoreResult<()> {
    conn.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
}

// ----- projects ---------------------------------------------------------------------------------

/// A stored project (git info is filled in by the caller from the filesystem).
#[derive(Debug, Clone)]
pub struct ProjectRow {
    pub project: Project,
    pub removed: bool,
}

const PROJECT_COLS: &str =
    "id, name, path, created_at, updated_at, archived, removed, defaults, harness_trust";

/// A project row as stored: the row without its JSON columns, `defaults` and `harness_trust`.
type RawProject = (ProjectRow, String, Option<String>);

fn project_row(r: &Row<'_>) -> rusqlite::Result<RawProject> {
    Ok((
        ProjectRow {
            project: Project {
                id: ProjectId::from(r.get::<_, String>(0)?),
                name: r.get(1)?,
                path: r.get(2)?,
                created_at: r.get(3)?,
                updated_at: r.get(4)?,
                archived: b(r.get(5)?),
                defaults: ProjectDefaults::default(),
                git: GitInfo::default(),
                harness_trust: Default::default(),
            },
            removed: b(r.get(6)?),
        },
        r.get::<_, String>(7)?,
        r.get(8)?,
    ))
}

fn finish_project((mut row, defaults, trust): RawProject) -> CoreResult<ProjectRow> {
    row.project.defaults = from_json(&defaults)?;
    row.project.harness_trust = opt_json(trust)?.unwrap_or_default();
    Ok(row)
}

pub fn get_project(conn: &Connection, id: &ProjectId) -> CoreResult<Option<ProjectRow>> {
    conn.query_row(
        &format!("SELECT {PROJECT_COLS} FROM projects WHERE id = ?1"),
        [id.as_str()],
        project_row,
    )
    .optional()?
    .map(finish_project)
    .transpose()
}

pub fn find_project_by_key(conn: &Connection, key: &str) -> CoreResult<Option<ProjectRow>> {
    conn.query_row(
        &format!("SELECT {PROJECT_COLS} FROM projects WHERE path_key = ?1"),
        [key],
        project_row,
    )
    .optional()?
    .map(finish_project)
    .transpose()
}

pub fn list_projects(conn: &Connection, include_archived: bool) -> CoreResult<Vec<Project>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {PROJECT_COLS} FROM projects WHERE removed = 0 AND (?1 OR archived = 0) ORDER BY updated_at DESC, id"
    ))?;
    let rows = stmt.query_map([include_archived], project_row)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(finish_project(row?)?.project);
    }
    Ok(out)
}

/// `projects.harness_trust`: `NULL` while no decision was recorded.
fn trust_json(p: &Project) -> Option<String> {
    (!p.harness_trust.is_empty()).then(|| to_json(&p.harness_trust))
}

pub fn insert_project(conn: &Connection, p: &Project, key: &str) -> CoreResult<()> {
    conn.execute(
        "INSERT INTO projects (id, name, path, path_key, created_at, updated_at, archived, removed, defaults, harness_trust)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0, ?8, ?9)",
        params![p.id.as_str(), p.name, p.path, key, p.created_at, p.updated_at, p.archived, to_json(&p.defaults), trust_json(p)],
    )?;
    Ok(())
}

pub fn update_project(conn: &Connection, p: &Project, removed: bool) -> CoreResult<()> {
    conn.execute(
        "UPDATE projects SET name = ?2, updated_at = ?3, archived = ?4, removed = ?5, defaults = ?6, harness_trust = ?7 WHERE id = ?1",
        params![p.id.as_str(), p.name, p.updated_at, p.archived, removed, to_json(&p.defaults), trust_json(p)],
    )?;
    Ok(())
}

// ----- threads ----------------------------------------------------------------------------------

/// A stored thread with its internal fields.
#[derive(Debug, Clone, PartialEq)]
pub struct ThreadRow {
    pub id: ThreadId,
    pub project_id: ProjectId,
    pub harness_id: String,
    pub title: String,
    /// `default`, `firstMessage`, `user`, `harness`, `fork`, `import`.
    pub title_source: String,
    pub cwd: String,
    pub workspace: Workspace,
    pub settings: ThreadSettings,
    pub status: ThreadStatus,
    pub native_session_id: Option<String>,
    /// Native session to fork from when the thread's first process starts.
    pub fork_source: Option<String>,
    pub forked_from: Option<ForkOrigin>,
    pub last_error: Option<ThreadError>,
    pub usage: Usage,
    /// Tree snapshot at the thread's first turn (thread-scope diff base).
    pub base_tree: Option<String>,
    pub diff_available: bool,
    pub queue_paused: bool,
    pub head: u64,
    pub created_at: Millis,
    pub updated_at: Millis,
    pub last_activity_at: Millis,
    pub archived: bool,
    pub removed: bool,
    pub pinned: bool,
    /// Plan mode and fast mode (`Thread.modes`).
    pub modes: ThreadModes,
    /// What the harness last reported about fast mode (`Thread.fastModeState`).
    pub fast_mode_state: Option<String>,
    /// With `fork_source`: where the fork branches the source (a fork at a turn); `None`: the
    /// whole source session.
    pub fork_at: Option<aas_harness::ForkPoint>,
    /// The user's title has not reached the native session yet: it is given when the next
    /// agent process starts (harnesses with the feature `rename`).
    pub native_rename_pending: bool,
}

const THREAD_COLS: &str = "id, project_id, harness_id, title, title_source, cwd, workspace, settings, status, \
    native_session_id, fork_source, forked_from, last_error, usage, base_tree, diff_available, queue_paused, head, \
    created_at, updated_at, last_activity_at, archived, removed, pinned, modes, fast_mode_state, fork_at, \
    native_rename_pending";

const THREAD_COL_COUNT: usize = 28;

fn thread_row(r: &Row<'_>) -> rusqlite::Result<[Value; THREAD_COL_COUNT]> {
    // Read raw values first; JSON decoding happens outside the rusqlite callback.
    let mut vals: [Value; THREAD_COL_COUNT] = Default::default();
    for (i, v) in vals.iter_mut().enumerate() {
        *v = match r.get_ref(i)? {
            rusqlite::types::ValueRef::Null => Value::Null,
            rusqlite::types::ValueRef::Integer(n) => Value::from(n),
            rusqlite::types::ValueRef::Real(f) => Value::from(f),
            rusqlite::types::ValueRef::Text(t) => {
                Value::String(String::from_utf8_lossy(t).into_owned())
            }
            rusqlite::types::ValueRef::Blob(_) => Value::Null,
        };
    }
    Ok(vals)
}

fn s(v: &Value) -> String {
    v.as_str().unwrap_or_default().to_owned()
}
fn os(v: &Value) -> Option<String> {
    v.as_str().map(str::to_owned)
}
fn i(v: &Value) -> i64 {
    v.as_i64().unwrap_or(0)
}

fn decode_thread(v: [Value; THREAD_COL_COUNT]) -> CoreResult<ThreadRow> {
    Ok(ThreadRow {
        id: ThreadId::from(s(&v[0])),
        project_id: ProjectId::from(s(&v[1])),
        harness_id: s(&v[2]),
        title: s(&v[3]),
        title_source: s(&v[4]),
        cwd: s(&v[5]),
        workspace: from_json(&s(&v[6]))?,
        settings: from_json(&s(&v[7]))?,
        status: ThreadStatus::parse(&s(&v[8]))
            .ok_or_else(|| CoreError::Corrupt(format!("thread status {}", v[8])))?,
        native_session_id: os(&v[9]),
        fork_source: os(&v[10]),
        forked_from: opt_json(os(&v[11]))?,
        last_error: opt_json(os(&v[12]))?,
        usage: from_json(&s(&v[13]))?,
        base_tree: os(&v[14]),
        diff_available: i(&v[15]) != 0,
        queue_paused: i(&v[16]) != 0,
        head: i(&v[17]) as u64,
        created_at: i(&v[18]),
        updated_at: i(&v[19]),
        last_activity_at: i(&v[20]),
        archived: i(&v[21]) != 0,
        removed: i(&v[22]) != 0,
        pinned: i(&v[23]) != 0,
        modes: opt_json(os(&v[24]))?.unwrap_or_default(),
        fast_mode_state: os(&v[25]),
        fork_at: opt_json(os(&v[26]))?,
        native_rename_pending: i(&v[27]) != 0,
    })
}

pub fn get_thread(conn: &Connection, id: &ThreadId) -> CoreResult<Option<ThreadRow>> {
    conn.query_row(
        &format!("SELECT {THREAD_COLS} FROM threads WHERE id = ?1"),
        [id.as_str()],
        thread_row,
    )
    .optional()?
    .map(decode_thread)
    .transpose()
}

pub fn insert_thread(conn: &Connection, t: &ThreadRow) -> CoreResult<()> {
    conn.execute(
        &format!("INSERT INTO threads ({THREAD_COLS}) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24,?25,?26,?27,?28)"),
        params![
            t.id.as_str(),
            t.project_id.as_str(),
            t.harness_id,
            t.title,
            t.title_source,
            t.cwd,
            to_json(&t.workspace),
            to_json(&t.settings),
            t.status.as_str(),
            t.native_session_id,
            t.fork_source,
            t.forked_from.as_ref().map(to_json),
            t.last_error.as_ref().map(to_json),
            to_json(&t.usage),
            t.base_tree,
            t.diff_available,
            t.queue_paused,
            t.head as i64,
            t.created_at,
            t.updated_at,
            t.last_activity_at,
            t.archived,
            t.removed,
            t.pinned,
            to_json(&t.modes),
            t.fast_mode_state,
            t.fork_at.as_ref().map(to_json),
            t.native_rename_pending,
        ],
    )?;
    Ok(())
}

pub fn update_thread(conn: &Connection, t: &ThreadRow) -> CoreResult<()> {
    conn.execute(
        "UPDATE threads SET title = ?2, title_source = ?3, cwd = ?4, workspace = ?5, settings = ?6, status = ?7,
            native_session_id = ?8, fork_source = ?9, forked_from = ?10, last_error = ?11, usage = ?12, base_tree = ?13,
            diff_available = ?14, queue_paused = ?15, head = ?16, updated_at = ?17, last_activity_at = ?18,
            archived = ?19, removed = ?20, pinned = ?21, modes = ?22, fast_mode_state = ?23, fork_at = ?24,
            native_rename_pending = ?25
         WHERE id = ?1",
        params![
            t.id.as_str(),
            t.title,
            t.title_source,
            t.cwd,
            to_json(&t.workspace),
            to_json(&t.settings),
            t.status.as_str(),
            t.native_session_id,
            t.fork_source,
            t.forked_from.as_ref().map(to_json),
            t.last_error.as_ref().map(to_json),
            to_json(&t.usage),
            t.base_tree,
            t.diff_available,
            t.queue_paused,
            t.head as i64,
            t.updated_at,
            t.last_activity_at,
            t.archived,
            t.removed,
            t.pinned,
            to_json(&t.modes),
            t.fast_mode_state,
            t.fork_at.as_ref().map(to_json),
            t.native_rename_pending,
        ],
    )?;
    Ok(())
}

/// Builds the protocol view of a thread (counts and last turn are read from the database).
pub fn thread_view(conn: &Connection, t: &ThreadRow) -> CoreResult<Thread> {
    let pending: i64 = conn.query_row(
        "SELECT count(*) FROM interactions WHERE thread_id = ?1 AND status = 'pending'",
        [t.id.as_str()],
        |r| r.get(0),
    )?;
    let queued: i64 = conn.query_row(
        "SELECT count(*) FROM queued_inputs WHERE thread_id = ?1",
        [t.id.as_str()],
        |r| r.get(0),
    )?;
    let last_turn = last_turn(conn, &t.id)?.map(|row| row.turn.summary());
    let background = thread_background(conn, &t.id)?;
    Ok(Thread {
        id: t.id.clone(),
        project_id: t.project_id.clone(),
        harness_id: t.harness_id.clone(),
        title: t.title.clone(),
        cwd: t.cwd.clone(),
        workspace: t.workspace.clone(),
        settings: t.settings.clone(),
        status: t.status,
        pending_interactions: pending as u32,
        queued_inputs: queued as u32,
        queue_paused: t.queue_paused,
        last_turn,
        last_error: t.last_error.clone(),
        native_session_id: t.native_session_id.clone(),
        forked_from: t.forked_from.clone(),
        usage: t.usage,
        diff_available: t.diff_available,
        created_at: t.created_at,
        updated_at: t.updated_at,
        last_activity_at: t.last_activity_at,
        archived: t.archived,
        pinned: t.pinned,
        background,
        modes: t.modes,
        fast_mode_state: t.fast_mode_state.clone(),
        head: t.head,
    })
}

/// Threads ordered by recent activity, paged with a `(lastActivityAt, id)` cursor.
pub fn list_threads(
    conn: &Connection,
    project: Option<&ProjectId>,
    include_archived: bool,
    limit: usize,
    before: Option<&ThreadCursor>,
) -> CoreResult<(Vec<ThreadRow>, bool)> {
    let (before_at, before_id) = match before {
        Some(c) => (c.last_activity_at, c.id.as_str().to_owned()),
        None => (i64::MAX, String::new()),
    };
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {THREAD_COLS} FROM threads
         WHERE removed = 0 AND (?1 OR archived = 0) AND (?2 IS NULL OR project_id = ?2)
           AND (last_activity_at < ?3 OR (last_activity_at = ?3 AND (?4 = '' OR id < ?4)))
         ORDER BY last_activity_at DESC, id DESC LIMIT ?5"
    ))?;
    let rows = stmt.query_map(
        params![
            include_archived,
            project.map(|p| p.as_str()),
            before_at,
            before_id,
            (limit + 1) as i64
        ],
        thread_row,
    )?;
    let mut out = Vec::new();
    for row in rows {
        out.push(decode_thread(row?)?);
    }
    let has_more = out.len() > limit;
    out.truncate(limit);
    Ok((out, has_more))
}

pub fn threads_with_status(
    conn: &Connection,
    statuses: &[ThreadStatus],
) -> CoreResult<Vec<ThreadRow>> {
    let mut out = Vec::new();
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {THREAD_COLS} FROM threads WHERE status = ?1"
    ))?;
    for st in statuses {
        let rows = stmt.query_map([st.as_str()], thread_row)?;
        for row in rows {
            out.push(decode_thread(row?)?);
        }
    }
    Ok(out)
}

/// Every thread that is not removed (optionally without the archived ones), most recently
/// active first.
pub fn all_threads(conn: &Connection, include_archived: bool) -> CoreResult<Vec<ThreadRow>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {THREAD_COLS} FROM threads WHERE removed = 0 AND (?1 OR archived = 0) ORDER BY last_activity_at DESC, id DESC"
    ))?;
    let rows = stmt.query_map([include_archived], thread_row)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(decode_thread(row?)?);
    }
    Ok(out)
}

/// Threads marked removed but still stored (see `recover`).
pub fn removed_threads(conn: &Connection) -> CoreResult<Vec<ThreadRow>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {THREAD_COLS} FROM threads WHERE removed = 1"
    ))?;
    let rows = stmt.query_map([], thread_row)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(decode_thread(row?)?);
    }
    Ok(out)
}

pub fn threads_of_project(conn: &Connection, project: &ProjectId) -> CoreResult<Vec<ThreadRow>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {THREAD_COLS} FROM threads WHERE project_id = ?1 AND removed = 0"
    ))?;
    let rows = stmt.query_map([project.as_str()], thread_row)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(decode_thread(row?)?);
    }
    Ok(out)
}

pub fn thread_by_native_session(
    conn: &Connection,
    harness: &str,
    native: &str,
) -> CoreResult<Option<ThreadId>> {
    Ok(conn
        .query_row(
            "SELECT id FROM threads WHERE harness_id = ?1 AND native_session_id = ?2 AND removed = 0 LIMIT 1",
            params![harness, native],
            |r| r.get::<_, String>(0),
        )
        .optional()?
        .map(ThreadId::from))
}

// ----- turns ------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct TurnRow {
    pub turn: Turn,
    /// Working-tree snapshot when the turn started.
    pub base_tree: Option<String>,
    /// Working-tree snapshot when the turn ended.
    pub end_tree: Option<String>,
    /// The harness's own anchor of the turn (`AdapterEvent::TurnAnchor`), kept to fork at it.
    /// `Turn.forkable` says whether it is set.
    pub native_anchor: Option<Value>,
    /// The native session `native_anchor` belongs to, once the thread has moved to another
    /// native session; `None`: the thread's current one (design.md §9.6).
    pub anchor_session: Option<String>,
    /// The turn's input reached the agent, or the agent started the turn itself: the turn is
    /// part of the native session. `false` for a turn whose start failed or that was
    /// interrupted before it was sent.
    pub delivered: bool,
}

impl TurnRow {
    /// A row without snapshots or anchor (`turn.forkable` follows the anchor), not delivered.
    pub fn new(turn: Turn) -> Self {
        Self {
            turn: Turn {
                forkable: false,
                ..turn
            },
            base_tree: None,
            end_tree: None,
            native_anchor: None,
            anchor_session: None,
            delivered: false,
        }
    }
}

const TURN_COLS: &str = "id, thread_id, idx, status, started_at, completed_at, model, error, usage, diff, base_tree, end_tree, start_trigger, native_anchor, anchor_session, delivered";

/// A turn row as stored: the row without its JSON and enum columns, those columns (error,
/// usage, diff, anchor), the status and the trigger.
type RawTurn = (TurnRow, [Option<String>; 4], String, Option<String>);

fn turn_row(r: &Row<'_>) -> rusqlite::Result<RawTurn> {
    Ok((
        TurnRow {
            turn: Turn {
                id: TurnId::from(r.get::<_, String>(0)?),
                thread_id: ThreadId::from(r.get::<_, String>(1)?),
                index: r.get::<_, i64>(2)? as u32,
                status: TurnStatus::Running,
                started_at: r.get(4)?,
                completed_at: r.get(5)?,
                model: r.get(6)?,
                error: None,
                usage: None,
                diff: None,
                trigger: None,
                forkable: false,
            },
            base_tree: r.get(10)?,
            end_tree: r.get(11)?,
            native_anchor: None,
            anchor_session: r.get(14)?,
            delivered: r.get(15)?,
        },
        [r.get(7)?, r.get(8)?, r.get(9)?, r.get(13)?],
        r.get(3)?,
        r.get(12)?,
    ))
}

fn finish_turn(
    (mut row, [error, usage, diff, anchor], status, trigger): RawTurn,
) -> CoreResult<TurnRow> {
    row.turn.status = TurnStatus::parse(&status)
        .ok_or_else(|| CoreError::Corrupt(format!("turn status {status}")))?;
    row.turn.error = opt_json(error)?;
    row.turn.usage = opt_json(usage)?;
    row.turn.diff = opt_json(diff)?;
    row.turn.trigger = trigger
        .map(|t| {
            TurnTrigger::parse(&t).ok_or_else(|| CoreError::Corrupt(format!("turn trigger {t}")))
        })
        .transpose()?;
    row.native_anchor = opt_json(anchor)?;
    row.turn.forkable = row.native_anchor.is_some();
    Ok(row)
}

pub fn insert_turn(conn: &Connection, t: &TurnRow) -> CoreResult<()> {
    conn.execute(
        &format!(
            "INSERT INTO turns ({TURN_COLS}) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)"
        ),
        params![
            t.turn.id.as_str(),
            t.turn.thread_id.as_str(),
            t.turn.index as i64,
            t.turn.status.as_str(),
            t.turn.started_at,
            t.turn.completed_at,
            t.turn.model,
            t.turn.error.as_ref().map(to_json),
            t.turn.usage.as_ref().map(to_json),
            t.turn.diff.as_ref().map(to_json),
            t.base_tree,
            t.end_tree,
            t.turn.trigger.map(TurnTrigger::as_str),
            t.native_anchor.as_ref().map(to_json),
            t.anchor_session,
            t.delivered,
        ],
    )?;
    Ok(())
}

pub fn update_turn(conn: &Connection, t: &TurnRow) -> CoreResult<()> {
    conn.execute(
        "UPDATE turns SET status = ?2, completed_at = ?3, model = ?4, error = ?5, usage = ?6, diff = ?7, base_tree = ?8,
            end_tree = ?9, start_trigger = ?10, native_anchor = ?11, anchor_session = ?12, delivered = ?13
         WHERE id = ?1",
        params![
            t.turn.id.as_str(),
            t.turn.status.as_str(),
            t.turn.completed_at,
            t.turn.model,
            t.turn.error.as_ref().map(to_json),
            t.turn.usage.as_ref().map(to_json),
            t.turn.diff.as_ref().map(to_json),
            t.base_tree,
            t.end_tree,
            t.turn.trigger.map(TurnTrigger::as_str),
            t.native_anchor.as_ref().map(to_json),
            t.anchor_session,
            t.delivered,
        ],
    )?;
    Ok(())
}

pub fn get_turn(conn: &Connection, id: &TurnId) -> CoreResult<Option<TurnRow>> {
    conn.query_row(
        &format!("SELECT {TURN_COLS} FROM turns WHERE id = ?1"),
        [id.as_str()],
        turn_row,
    )
    .optional()?
    .map(finish_turn)
    .transpose()
}

pub fn last_turn(conn: &Connection, thread: &ThreadId) -> CoreResult<Option<TurnRow>> {
    conn.query_row(
        &format!("SELECT {TURN_COLS} FROM turns WHERE thread_id = ?1 ORDER BY idx DESC LIMIT 1"),
        [thread.as_str()],
        turn_row,
    )
    .optional()?
    .map(finish_turn)
    .transpose()
}

/// Turns before `before_index` (newest first), returned oldest first.
pub fn list_turns(
    conn: &Connection,
    thread: &ThreadId,
    before_index: Option<u32>,
    limit: usize,
) -> CoreResult<(Vec<TurnRow>, bool)> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {TURN_COLS} FROM turns WHERE thread_id = ?1 AND idx < ?2 ORDER BY idx DESC LIMIT ?3"
    ))?;
    let rows = stmt.query_map(
        params![
            thread.as_str(),
            before_index.map(i64::from).unwrap_or(i64::MAX),
            (limit + 1) as i64
        ],
        turn_row,
    )?;
    let mut out = Vec::new();
    for row in rows {
        out.push(finish_turn(row?)?);
    }
    let has_more = out.len() > limit;
    out.truncate(limit);
    out.reverse();
    Ok((out, has_more))
}

/// Every turn of `thread`, oldest first.
pub fn all_turns(conn: &Connection, thread: &ThreadId) -> CoreResult<Vec<TurnRow>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {TURN_COLS} FROM turns WHERE thread_id = ?1 ORDER BY idx"
    ))?;
    let rows = stmt.query_map([thread.as_str()], turn_row)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(finish_turn(row?)?);
    }
    Ok(out)
}

/// Gives the turn of `thread` whose recorded anchor is exactly `previous` the anchor `anchor`
/// (see `AdapterEvent::TurnAnchorReplaced`). Only anchors of the thread's current native session
/// are candidates: the harness names the anchors of the session it runs. Returns whether a turn
/// had it.
pub fn replace_turn_anchor(
    conn: &Connection,
    thread: &ThreadId,
    previous: &Value,
    anchor: &Value,
) -> CoreResult<bool> {
    let rows: Vec<(String, String)> = conn
        .prepare_cached(
            "SELECT id, native_anchor FROM turns
             WHERE thread_id = ?1 AND native_anchor IS NOT NULL AND anchor_session IS NULL",
        )?
        .query_map([thread.as_str()], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let mut replaced = false;
    for (id, stored) in rows {
        let stored: Value = from_json(&stored)?;
        if stored == *previous {
            conn.execute(
                "UPDATE turns SET native_anchor = ?2 WHERE id = ?1",
                params![id, to_json(anchor)],
            )?;
            replaced = true;
        }
    }
    Ok(replaced)
}

/// The thread moved from native session `previous` to another one: the anchors recorded so far
/// (those of its current session, `anchor_session IS NULL`) are kept as anchors of `previous`.
pub fn keep_anchors_with_session(
    conn: &Connection,
    thread: &ThreadId,
    previous: &str,
) -> CoreResult<()> {
    conn.execute(
        "UPDATE turns SET anchor_session = ?2
         WHERE thread_id = ?1 AND native_anchor IS NOT NULL AND anchor_session IS NULL",
        params![thread.as_str(), previous],
    )?;
    Ok(())
}

pub fn turns_with_status(conn: &Connection, status: TurnStatus) -> CoreResult<Vec<TurnRow>> {
    let mut stmt =
        conn.prepare_cached(&format!("SELECT {TURN_COLS} FROM turns WHERE status = ?1"))?;
    let rows = stmt.query_map([status.as_str()], turn_row)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(finish_turn(row?)?);
    }
    Ok(out)
}

pub fn next_turn_index(conn: &Connection, thread: &ThreadId) -> CoreResult<u32> {
    let n: i64 = conn.query_row(
        "SELECT COALESCE(MAX(idx), -1) + 1 FROM turns WHERE thread_id = ?1",
        [thread.as_str()],
        |r| r.get(0),
    )?;
    Ok(n as u32)
}

// ----- items ------------------------------------------------------------------------------------

const ITEM_COLS: &str = "id, thread_id, turn_id, status, started_at, completed_at, body, background_task_id, backgroundable";

/// An item row as stored (id, thread, turn, status, started, completed, body, background task,
/// backgroundable).
type RawItem = (
    String,
    String,
    String,
    String,
    i64,
    Option<i64>,
    String,
    Option<String>,
    i64,
);

fn item_row(r: &Row<'_>) -> rusqlite::Result<RawItem> {
    Ok((
        r.get(0)?,
        r.get(1)?,
        r.get(2)?,
        r.get(3)?,
        r.get(4)?,
        r.get(5)?,
        r.get(6)?,
        r.get(7)?,
        r.get(8)?,
    ))
}

fn finish_item(
    (id, thread, turn, status, started, completed, body, task, backgroundable): RawItem,
) -> CoreResult<Item> {
    Ok(Item {
        id: ItemId::from(id),
        thread_id: ThreadId::from(thread),
        turn_id: TurnId::from(turn),
        status: ItemStatus::parse(&status)
            .ok_or_else(|| CoreError::Corrupt(format!("item status {status}")))?,
        started_at: started,
        completed_at: completed,
        background_task_id: task.map(BackgroundTaskId::from),
        backgroundable: b(backgroundable),
        body: from_json(&body)?,
    })
}

pub fn insert_item(conn: &Connection, item: &Item) -> CoreResult<()> {
    sync_item_blob_refs(conn, item)?;
    let ord: i64 = conn.query_row(
        "SELECT COALESCE(MAX(ord), 0) + 1 FROM items WHERE thread_id = ?1",
        [item.thread_id.as_str()],
        |r| r.get(0),
    )?;
    conn.execute(
        &format!("INSERT INTO items ({ITEM_COLS}, ord) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)"),
        params![
            item.id.as_str(),
            item.thread_id.as_str(),
            item.turn_id.as_str(),
            item.status.as_str(),
            item.started_at,
            item.completed_at,
            to_json(&item.body),
            item.background_task_id.as_ref().map(|t| t.as_str()),
            item.backgroundable,
            ord,
        ],
    )?;
    Ok(())
}

pub fn update_item(conn: &Connection, item: &Item) -> CoreResult<()> {
    sync_item_blob_refs(conn, item)?;
    conn.execute(
        "UPDATE items SET status = ?2, completed_at = ?3, body = ?4, background_task_id = ?5, backgroundable = ?6 WHERE id = ?1",
        params![
            item.id.as_str(),
            item.status.as_str(),
            item.completed_at,
            to_json(&item.body),
            item.background_task_id.as_ref().map(|t| t.as_str()),
            item.backgroundable,
        ],
    )?;
    Ok(())
}

pub fn get_item(conn: &Connection, id: &ItemId) -> CoreResult<Option<Item>> {
    conn.query_row(
        &format!("SELECT {ITEM_COLS} FROM items WHERE id = ?1"),
        [id.as_str()],
        item_row,
    )
    .optional()?
    .map(finish_item)
    .transpose()
}

pub fn items_of_turns(conn: &Connection, turns: &[TurnId]) -> CoreResult<Vec<Item>> {
    let mut out = Vec::new();
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {ITEM_COLS} FROM items WHERE turn_id = ?1 ORDER BY ord"
    ))?;
    for turn in turns {
        let rows = stmt.query_map([turn.as_str()], item_row)?;
        for row in rows {
            out.push(finish_item(row?)?);
        }
    }
    Ok(out)
}

pub fn items_in_progress(conn: &Connection) -> CoreResult<Vec<Item>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {ITEM_COLS} FROM items WHERE status = 'inProgress'"
    ))?;
    let rows = stmt.query_map([], item_row)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(finish_item(row?)?);
    }
    Ok(out)
}

// ----- interactions -----------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct InteractionRow {
    pub interaction: Interaction,
    pub adapter_request_id: String,
    /// For an interaction that belongs to no turn (a background task's, or the thread's): the
    /// turn that ran when it was asked, or else the thread's last turn. `thread/read` returns
    /// the interaction with that turn.
    pub anchor_turn_id: Option<TurnId>,
}

const INTERACTION_COLS: &str = "id, thread_id, turn_id, item_id, status, created_at, resolved_at, resolved_by, request, resolution, expire_reason, adapter_request_id, background_task_id, anchor_turn_id";

type RawInteraction = (
    String,
    String,
    Option<String>,
    Option<String>,
    String,
    i64,
    Option<i64>,
    Option<String>,
    String,
    Option<String>,
    Option<String>,
    String,
    Option<String>,
    Option<String>,
);

fn interaction_row(r: &Row<'_>) -> rusqlite::Result<RawInteraction> {
    Ok((
        r.get(0)?,
        r.get(1)?,
        r.get(2)?,
        r.get(3)?,
        r.get(4)?,
        r.get(5)?,
        r.get(6)?,
        r.get(7)?,
        r.get(8)?,
        r.get(9)?,
        r.get(10)?,
        r.get(11)?,
        r.get(12)?,
        r.get(13)?,
    ))
}

fn finish_interaction(raw: RawInteraction) -> CoreResult<InteractionRow> {
    let (
        id,
        thread,
        turn,
        item,
        status,
        created,
        resolved_at,
        resolved_by,
        request,
        resolution,
        expire,
        adapter,
        task,
        anchor,
    ) = raw;
    Ok(InteractionRow {
        interaction: Interaction {
            id: InteractionId::from(id),
            thread_id: ThreadId::from(thread),
            turn_id: turn.map(TurnId::from),
            item_id: item.map(ItemId::from),
            background_task_id: task.map(BackgroundTaskId::from),
            status: InteractionStatus::parse(&status)
                .ok_or_else(|| CoreError::Corrupt(format!("interaction status {status}")))?,
            created_at: created,
            resolved_at,
            resolved_by,
            request: from_json(&request)?,
            resolution: opt_json(resolution)?,
            expire_reason: expire.map(|e| from_json(&format!("\"{e}\""))).transpose()?,
        },
        adapter_request_id: adapter,
        anchor_turn_id: anchor.map(TurnId::from),
    })
}

fn expire_reason_str(r: &ExpireReason) -> String {
    to_json(r).trim_matches('"').to_owned()
}

pub fn insert_interaction(conn: &Connection, row: &InteractionRow) -> CoreResult<()> {
    let i = &row.interaction;
    conn.execute(
        &format!("INSERT INTO interactions ({INTERACTION_COLS}) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)"),
        params![
            i.id.as_str(),
            i.thread_id.as_str(),
            i.turn_id.as_ref().map(|t| t.as_str()),
            i.item_id.as_ref().map(|t| t.as_str()),
            i.status.as_str(),
            i.created_at,
            i.resolved_at,
            i.resolved_by,
            to_json(&i.request),
            i.resolution.as_ref().map(to_json),
            i.expire_reason.as_ref().map(expire_reason_str),
            row.adapter_request_id,
            i.background_task_id.as_ref().map(|t| t.as_str()),
            row.anchor_turn_id.as_ref().map(|t| t.as_str()),
        ],
    )?;
    Ok(())
}

pub fn update_interaction(conn: &Connection, i: &Interaction) -> CoreResult<()> {
    conn.execute(
        "UPDATE interactions SET status = ?2, resolved_at = ?3, resolved_by = ?4, resolution = ?5, expire_reason = ?6
         WHERE id = ?1",
        params![
            i.id.as_str(),
            i.status.as_str(),
            i.resolved_at,
            i.resolved_by,
            i.resolution.as_ref().map(to_json),
            i.expire_reason.as_ref().map(expire_reason_str),
        ],
    )?;
    Ok(())
}

pub fn get_interaction(
    conn: &Connection,
    id: &InteractionId,
) -> CoreResult<Option<InteractionRow>> {
    conn.query_row(
        &format!("SELECT {INTERACTION_COLS} FROM interactions WHERE id = ?1"),
        [id.as_str()],
        interaction_row,
    )
    .optional()?
    .map(finish_interaction)
    .transpose()
}

pub fn pending_interactions(conn: &Connection) -> CoreResult<Vec<InteractionRow>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {INTERACTION_COLS} FROM interactions WHERE status = 'pending' ORDER BY created_at"
    ))?;
    let rows = stmt.query_map([], interaction_row)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(finish_interaction(row?)?);
    }
    Ok(out)
}

/// The interactions `thread/read` returns with `turns` (in turn order, each turn's in the order
/// they were asked): those of the turns, those asked during them that belong to a background
/// task or to the thread, and then every other pending one of the thread that belongs to no
/// turn (asked during a turn outside the page, it still waits for an answer).
pub fn interactions_for_read(
    conn: &Connection,
    thread: &ThreadId,
    turns: &[TurnId],
) -> CoreResult<Vec<Interaction>> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {INTERACTION_COLS} FROM interactions WHERE turn_id = ?1 OR anchor_turn_id = ?1 ORDER BY created_at, id"
    ))?;
    for turn in turns {
        let rows = stmt.query_map([turn.as_str()], interaction_row)?;
        for row in rows {
            let interaction = finish_interaction(row?)?.interaction;
            if seen.insert(interaction.id.clone()) {
                out.push(interaction);
            }
        }
    }
    let mut pending = conn.prepare_cached(&format!(
        "SELECT {INTERACTION_COLS} FROM interactions
         WHERE thread_id = ?1 AND status = 'pending' AND turn_id IS NULL ORDER BY created_at, id"
    ))?;
    let rows = pending.query_map([thread.as_str()], interaction_row)?;
    for row in rows {
        let interaction = finish_interaction(row?)?.interaction;
        if seen.insert(interaction.id.clone()) {
            out.push(interaction);
        }
    }
    Ok(out)
}

// ----- background tasks -------------------------------------------------------------------------

/// Inserts a background task or replaces the stored state of one (whole object), with the
/// reference to its spilled output. An ended task's end becomes the thread's last one
/// ([`thread_background`]) unless the thread has a later one.
pub fn upsert_background_task(conn: &Connection, task: &BackgroundTask) -> CoreResult<()> {
    note_background_end(conn, task)?;
    conn.execute(
        "INSERT INTO background_tasks (id, thread_id, status, ambient, turn_id, started_at, ended_at, task)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         ON CONFLICT(id) DO UPDATE SET status = excluded.status, ambient = excluded.ambient,
           turn_id = excluded.turn_id, started_at = excluded.started_at, ended_at = excluded.ended_at,
           task = excluded.task",
        params![
            task.id.as_str(),
            task.thread_id.as_str(),
            task.status.as_str(),
            task.ambient,
            task.turn_id.as_ref().map(|t| t.as_str()),
            task.started_at,
            task.ended_at,
            to_json(task),
        ],
    )?;
    let blobs: Vec<BlobId> = task
        .result
        .as_ref()
        .and_then(|r| r.output_blob_id.clone())
        .into_iter()
        .collect();
    set_blob_refs(
        conn,
        BlobOwner::BackgroundTask,
        task.id.as_str(),
        &task.thread_id,
        &blobs,
        now_ms(),
    )
}

fn background_tasks_where(
    conn: &Connection,
    condition: &str,
    params: impl rusqlite::Params,
) -> CoreResult<Vec<BackgroundTask>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT task FROM background_tasks WHERE {condition} ORDER BY started_at, id"
    ))?;
    let rows = stmt.query_map(params, |r| r.get::<_, String>(0))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(from_json(&row?)?);
    }
    Ok(out)
}

pub fn get_background_task(
    conn: &Connection,
    id: &BackgroundTaskId,
) -> CoreResult<Option<BackgroundTask>> {
    Ok(background_tasks_where(conn, "id = ?1", [id.as_str()])?
        .into_iter()
        .next())
}

/// The background tasks `thread/read` returns with `turns`: those first reported during them,
/// and every task of the thread that is still running (oldest first).
pub fn background_tasks_for_read(
    conn: &Connection,
    thread: &ThreadId,
    turns: &[TurnId],
) -> CoreResult<Vec<BackgroundTask>> {
    let mut out = background_tasks_where(
        conn,
        "thread_id = ?1 AND status = 'running'",
        [thread.as_str()],
    )?;
    let mut seen: std::collections::HashSet<BackgroundTaskId> =
        out.iter().map(|t| t.id.clone()).collect();
    for turn in turns {
        for task in background_tasks_where(conn, "turn_id = ?1", [turn.as_str()])? {
            if seen.insert(task.id.clone()) {
                out.push(task);
            }
        }
    }
    out.sort_by(|a, b| (a.started_at, &a.id).cmp(&(b.started_at, &b.id)));
    Ok(out)
}

/// Every task still marked running (startup recovery).
pub fn running_background_tasks(conn: &Connection) -> CoreResult<Vec<BackgroundTask>> {
    background_tasks_where(conn, "status = 'running'", [])
}

/// Records the end of `task` as its thread's last end (`Thread.background.lastEnded`) when the
/// task has ended, is not ambient, and the thread has no later end: the latest `endedAt`, then
/// the greatest task id (a total order, so the same ends give the same answer in any order of
/// writing). A repeated write of the same end replaces it (its status or title may still
/// change).
///
/// * The record is kept apart from the task because a task that starts a new run under the
///   same id clears its own `ended_at`: the summary must not go back to an older end then, or
///   clients — which announce a finished task when `lastEnded` changes — would announce that
///   older end again (design.md §5.6).
/// * Ambient work is left out: the harness says it is not activity (hosts keep it out of
///   activity indicators), so its end — reported by the harness, or with the process when it
///   is reaped idle or lost — must not be reported as the end of the thread's work.
fn note_background_end(conn: &Connection, task: &BackgroundTask) -> CoreResult<()> {
    let Some(ended_at) = task
        .ended_at
        .filter(|_| task.status.is_terminal() && !task.ambient)
    else {
        return Ok(());
    };
    let ended = BackgroundTaskEnded {
        task_id: task.id.clone(),
        title: task.title.clone(),
        kind: task.kind,
        status: task.status,
        ended_at,
    };
    conn.execute(
        "INSERT INTO background_last_ended (thread_id, ended_at, task_id, ended) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(thread_id) DO UPDATE SET ended_at = excluded.ended_at,
           task_id = excluded.task_id, ended = excluded.ended
         WHERE (excluded.ended_at, excluded.task_id)
           >= (background_last_ended.ended_at, background_last_ended.task_id)",
        params![
            task.thread_id.as_str(),
            ended_at,
            task.id.as_str(),
            to_json(&ended),
        ],
    )?;
    Ok(())
}

/// Fills the threads' last ends from the tasks stored before they were kept apart (schema v5).
pub fn backfill_background_last_ended(conn: &Connection) -> CoreResult<()> {
    for task in background_tasks_where(conn, "ended_at IS NOT NULL", [])? {
        note_background_end(conn, &task)?;
    }
    Ok(())
}

/// `Thread.background`: the running tasks that are not ambient, and the last end the thread's
/// background work reached ([`note_background_end`]: never an ambient task's, and never older
/// than an end already reported).
pub fn thread_background(conn: &Connection, thread: &ThreadId) -> CoreResult<ThreadBackground> {
    let running: i64 = conn.query_row(
        "SELECT count(*) FROM background_tasks WHERE thread_id = ?1 AND status = 'running' AND ambient = 0",
        [thread.as_str()],
        |r| r.get(0),
    )?;
    let last: Option<String> = conn
        .query_row(
            "SELECT ended FROM background_last_ended WHERE thread_id = ?1",
            [thread.as_str()],
            |r| r.get(0),
        )
        .optional()?;
    Ok(ThreadBackground {
        running: running as u32,
        last_ended: opt_json(last)?,
    })
}

// ----- queued inputs ----------------------------------------------------------------------------

pub fn insert_queued(conn: &Connection, q: &QueuedInput) -> CoreResult<()> {
    conn.execute(
        "INSERT INTO queued_inputs (id, thread_id, created_at, input) VALUES (?1, ?2, ?3, ?4)",
        params![
            q.id.as_str(),
            q.thread_id.as_str(),
            q.created_at,
            to_json(&q.input)
        ],
    )?;
    set_blob_refs(
        conn,
        BlobOwner::Queued,
        q.id.as_str(),
        &q.thread_id,
        &input_blob_ids(&q.input),
        now_ms(),
    )
}

/// Replaces the input of a queued entry (its place in the queue is kept).
pub fn update_queued(
    conn: &Connection,
    id: &QueuedInputId,
    input: &[InputPart],
) -> CoreResult<bool> {
    let thread: Option<String> = conn
        .query_row(
            "SELECT thread_id FROM queued_inputs WHERE id = ?1",
            [id.as_str()],
            |r| r.get(0),
        )
        .optional()?;
    let Some(thread) = thread else {
        return Ok(false);
    };
    conn.execute(
        "UPDATE queued_inputs SET input = ?2 WHERE id = ?1",
        params![id.as_str(), to_json(&input)],
    )?;
    set_blob_refs(
        conn,
        BlobOwner::Queued,
        id.as_str(),
        &ThreadId::from(thread),
        &input_blob_ids(input),
        now_ms(),
    )?;
    Ok(true)
}

pub fn delete_queued(conn: &Connection, id: &QueuedInputId) -> CoreResult<bool> {
    let deleted = conn.execute("DELETE FROM queued_inputs WHERE id = ?1", [id.as_str()])? > 0;
    if deleted {
        release_blob_refs(conn, BlobOwner::Queued, id.as_str(), now_ms())?;
    }
    Ok(deleted)
}

/// The queued inputs of `thread` in order, each with a preview of `preview_chars` characters
/// (`policy.queued_preview_chars`).
pub fn list_queued(
    conn: &Connection,
    thread: &ThreadId,
    preview_chars: usize,
) -> CoreResult<Vec<QueuedInput>> {
    let mut stmt = conn.prepare_cached(
        "SELECT id, thread_id, created_at, input FROM queued_inputs WHERE thread_id = ?1 ORDER BY created_at, id",
    )?;
    let rows = stmt.query_map([thread.as_str()], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, i64>(2)?,
            r.get::<_, String>(3)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (id, thread, created_at, input) = row?;
        let input: Vec<InputPart> = from_json(&input)?;
        out.push(QueuedInput {
            id: QueuedInputId::from(id),
            thread_id: ThreadId::from(thread),
            created_at,
            preview: input_preview(&input, preview_chars),
            input,
        });
    }
    Ok(out)
}

/// First line of the text parts, capped at `max_chars` characters
/// (`policy.queued_preview_chars`).
pub fn input_preview(input: &[InputPart], max_chars: usize) -> String {
    let text: String = input
        .iter()
        .filter_map(|p| match p {
            InputPart::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" ");
    let first = text
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    let mut out: String = first.chars().take(max_chars).collect();
    if first.chars().count() > max_chars {
        out.push('…');
    }
    out
}

// ----- devices & pairing ------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceRow {
    pub id: DeviceId,
    pub name: String,
    pub platform: Option<String>,
    pub created_at: Millis,
    pub last_seen_at: Option<Millis>,
    pub revoked: bool,
}

pub fn insert_device(conn: &Connection, d: &DeviceRow, token_hash: &str) -> CoreResult<()> {
    conn.execute(
        "INSERT INTO devices (id, name, platform, token_hash, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![d.id.as_str(), d.name, d.platform, token_hash, d.created_at],
    )?;
    Ok(())
}

fn device_row(r: &Row<'_>) -> rusqlite::Result<DeviceRow> {
    Ok(DeviceRow {
        id: DeviceId::from(r.get::<_, String>(0)?),
        name: r.get(1)?,
        platform: r.get(2)?,
        created_at: r.get(3)?,
        last_seen_at: r.get(4)?,
        revoked: r.get::<_, Option<i64>>(5)?.is_some(),
    })
}

pub fn device_by_token_hash(conn: &Connection, hash: &str) -> CoreResult<Option<DeviceRow>> {
    Ok(conn
        .query_row(
            "SELECT id, name, platform, created_at, last_seen_at, revoked_at FROM devices WHERE token_hash = ?1",
            [hash],
            device_row,
        )
        .optional()?)
}

pub fn list_devices(conn: &Connection) -> CoreResult<Vec<DeviceRow>> {
    let mut stmt = conn.prepare_cached(
        "SELECT id, name, platform, created_at, last_seen_at, revoked_at FROM devices WHERE revoked_at IS NULL ORDER BY created_at",
    )?;
    let rows = stmt.query_map([], device_row)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

pub fn revoke_device(conn: &Connection, id: &DeviceId, now: Millis) -> CoreResult<bool> {
    Ok(conn.execute(
        "UPDATE devices SET revoked_at = ?2 WHERE id = ?1 AND revoked_at IS NULL",
        params![id.as_str(), now],
    )? > 0)
}

pub fn touch_device(conn: &Connection, id: &DeviceId, now: Millis) -> CoreResult<()> {
    conn.execute(
        "UPDATE devices SET last_seen_at = ?2 WHERE id = ?1",
        params![id.as_str(), now],
    )?;
    Ok(())
}

pub fn insert_pairing_code(
    conn: &Connection,
    hash: &str,
    now: Millis,
    expires_at: Millis,
) -> CoreResult<()> {
    conn.execute(
        "INSERT INTO pairing_codes (code_hash, created_at, expires_at) VALUES (?1, ?2, ?3)",
        params![hash, now, expires_at],
    )?;
    Ok(())
}

/// Marks a valid, unused, unexpired code as used. Returns whether it was valid.
pub fn consume_pairing_code(conn: &Connection, hash: &str, now: Millis) -> CoreResult<bool> {
    Ok(conn.execute(
        "UPDATE pairing_codes SET used_at = ?2 WHERE code_hash = ?1 AND used_at IS NULL AND expires_at > ?2",
        params![hash, now],
    )? > 0)
}

pub fn gc_pairing_codes(conn: &Connection, now: Millis) -> CoreResult<usize> {
    Ok(conn.execute(
        "DELETE FROM pairing_codes WHERE expires_at <= ?1 OR used_at IS NOT NULL",
        [now],
    )?)
}

// ----- blobs ------------------------------------------------------------------------------------

/// Records a stored blob. A blob nothing refers to yet (an upload, a patch, a spill whose item
/// is written after it in the same transaction) starts its grace period now; storing the same
/// content again restarts the grace period of a blob that is still unreferenced.
pub fn insert_blob(
    conn: &Connection,
    id: &BlobId,
    mime: &str,
    size: u64,
    now: Millis,
) -> CoreResult<()> {
    conn.execute(
        "INSERT INTO blobs (id, mime, size, created_at, orphaned_at)
         VALUES (?1, ?2, ?3, ?4, CASE WHEN EXISTS (SELECT 1 FROM blob_refs WHERE blob_id = ?1) THEN NULL ELSE ?4 END)
         ON CONFLICT(id) DO UPDATE SET orphaned_at = CASE WHEN blobs.orphaned_at IS NULL THEN NULL ELSE excluded.orphaned_at END",
        params![id.as_str(), mime, size as i64, now],
    )?;
    Ok(())
}

/// What holds a reference to a blob.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlobOwner {
    /// An item (a user message's image, a command's or tool's spilled output).
    Item,
    /// A queued input's image.
    Queued,
    /// A background task's spilled output.
    BackgroundTask,
}

impl BlobOwner {
    fn as_str(self) -> &'static str {
        match self {
            BlobOwner::Item => "item",
            BlobOwner::Queued => "queued",
            BlobOwner::BackgroundTask => "backgroundTask",
        }
    }
}

/// Blobs an item body refers to (read from its typed fields).
pub fn item_blob_ids(body: &ItemBody) -> Vec<BlobId> {
    match body {
        ItemBody::UserMessage { attachments, .. } => attachments
            .iter()
            .map(|a| match a {
                Attachment::Image { blob_id, .. } => blob_id.clone(),
            })
            .collect(),
        ItemBody::CommandExecution { output_blob_id, .. }
        | ItemBody::ToolCall { output_blob_id, .. } => output_blob_id.iter().cloned().collect(),
        ItemBody::AgentMessage { .. }
        | ItemBody::Reasoning { .. }
        | ItemBody::FileChange { .. }
        | ItemBody::Plan { .. }
        | ItemBody::Notice { .. }
        | ItemBody::ProposedPlan { .. } => Vec::new(),
    }
}

/// Whether items of this kind can refer to blobs at all (the others skip the bookkeeping).
fn item_kind_holds_blobs(body: &ItemBody) -> bool {
    matches!(
        body,
        ItemBody::UserMessage { .. }
            | ItemBody::CommandExecution { .. }
            | ItemBody::ToolCall { .. }
    )
}

/// Blobs an input refers to.
pub fn input_blob_ids(input: &[InputPart]) -> Vec<BlobId> {
    input
        .iter()
        .filter_map(|p| match p {
            InputPart::Image { blob_id } => Some(blob_id.clone()),
            InputPart::Text { .. } | InputPart::Mention { .. } => None,
        })
        .collect()
}

fn sync_item_blob_refs(conn: &Connection, item: &Item) -> CoreResult<()> {
    if !item_kind_holds_blobs(&item.body) {
        return Ok(());
    }
    set_blob_refs(
        conn,
        BlobOwner::Item,
        item.id.as_str(),
        &item.thread_id,
        &item_blob_ids(&item.body),
        now_ms(),
    )
}

/// Makes `blobs` the references `owner` holds: a reference it no longer holds is released (a
/// blob left without any starts its grace period), a new one ends the grace period.
pub fn set_blob_refs(
    conn: &Connection,
    kind: BlobOwner,
    owner: &str,
    thread: &ThreadId,
    blobs: &[BlobId],
    now: Millis,
) -> CoreResult<()> {
    let current: std::collections::BTreeSet<String> = conn
        .prepare_cached("SELECT blob_id FROM blob_refs WHERE owner_kind = ?1 AND owner_id = ?2")?
        .query_map(params![kind.as_str(), owner], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<_>>()?;
    let wanted: std::collections::BTreeSet<String> =
        blobs.iter().map(|b| b.as_str().to_owned()).collect();
    if current == wanted {
        return Ok(());
    }
    for gone in current.difference(&wanted) {
        conn.execute(
            "DELETE FROM blob_refs WHERE owner_kind = ?1 AND owner_id = ?2 AND blob_id = ?3",
            params![kind.as_str(), owner, gone],
        )?;
        mark_if_orphaned(conn, gone, now)?;
    }
    for new in wanted.difference(&current) {
        conn.execute(
            "INSERT INTO blob_refs (blob_id, owner_kind, owner_id, thread_id) VALUES (?1, ?2, ?3, ?4)",
            params![new, kind.as_str(), owner, thread.as_str()],
        )?;
        conn.execute("UPDATE blobs SET orphaned_at = NULL WHERE id = ?1", [new])?;
    }
    Ok(())
}

/// Releases every reference `owner` holds.
pub fn release_blob_refs(
    conn: &Connection,
    kind: BlobOwner,
    owner: &str,
    now: Millis,
) -> CoreResult<()> {
    let released: Vec<String> = conn
        .prepare_cached("SELECT blob_id FROM blob_refs WHERE owner_kind = ?1 AND owner_id = ?2")?
        .query_map(params![kind.as_str(), owner], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<_>>()?;
    conn.execute(
        "DELETE FROM blob_refs WHERE owner_kind = ?1 AND owner_id = ?2",
        params![kind.as_str(), owner],
    )?;
    for blob in released {
        mark_if_orphaned(conn, &blob, now)?;
    }
    Ok(())
}

/// Starts the grace period of `blob` when nothing refers to it any more.
fn mark_if_orphaned(conn: &Connection, blob: &str, now: Millis) -> CoreResult<()> {
    conn.execute(
        "UPDATE blobs SET orphaned_at = ?2
         WHERE id = ?1 AND orphaned_at IS NULL AND NOT EXISTS (SELECT 1 FROM blob_refs WHERE blob_id = ?1)",
        params![blob, now],
    )?;
    Ok(())
}

/// Records the references of the items and queued inputs stored before references were
/// recorded (schema v3), and starts the grace period of the blobs nothing refers to.
pub fn backfill_blob_refs(conn: &Connection, now: Millis) -> CoreResult<()> {
    let items: Vec<(String, String, String)> = conn
        .prepare("SELECT id, thread_id, body FROM items")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?;
    for (id, thread, body) in items {
        let body: ItemBody = from_json(&body)?;
        let blobs = item_blob_ids(&body);
        if !blobs.is_empty() {
            set_blob_refs(
                conn,
                BlobOwner::Item,
                &id,
                &ThreadId::from(thread),
                &blobs,
                now,
            )?;
        }
    }
    let queued: Vec<(String, String, String)> = conn
        .prepare("SELECT id, thread_id, input FROM queued_inputs")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?;
    for (id, thread, input) in queued {
        let input: Vec<InputPart> = from_json(&input)?;
        let blobs = input_blob_ids(&input);
        if !blobs.is_empty() {
            set_blob_refs(
                conn,
                BlobOwner::Queued,
                &id,
                &ThreadId::from(thread),
                &blobs,
                now,
            )?;
        }
    }
    conn.execute(
        "UPDATE blobs SET orphaned_at = ?1
         WHERE orphaned_at IS NULL AND NOT EXISTS (SELECT 1 FROM blob_refs WHERE blob_id = blobs.id)",
        [now],
    )?;
    Ok(())
}

/// Unreferenced blobs whose grace period began before `before`, oldest first, at most `limit`.
pub fn expired_blobs(conn: &Connection, before: Millis, limit: usize) -> CoreResult<Vec<BlobId>> {
    let rows: Vec<String> = conn
        .prepare_cached("SELECT id FROM blobs WHERE orphaned_at IS NOT NULL AND orphaned_at < ?1 ORDER BY orphaned_at LIMIT ?2")?
        .query_map(params![before, limit as i64], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows
        .into_iter()
        .map(|id| BlobId::from_sha256_hex(id.strip_prefix(BlobId::PREFIX).unwrap_or(&id)))
        .collect())
}

/// Deletes the record of `blob` if it is still unreferenced and its grace period began before
/// `before`. Returns its size when it was deleted.
pub fn delete_expired_blob(
    conn: &Connection,
    blob: &BlobId,
    before: Millis,
) -> CoreResult<Option<u64>> {
    let size: Option<i64> = conn
        .query_row(
            "DELETE FROM blobs
             WHERE id = ?1 AND orphaned_at IS NOT NULL AND orphaned_at < ?2
               AND NOT EXISTS (SELECT 1 FROM blob_refs WHERE blob_id = ?1)
             RETURNING size",
            params![blob.as_str(), before],
            |r| r.get(0),
        )
        .optional()?;
    Ok(size.map(|s| s as u64))
}

/// Which of `ids` have a record (a file without one was left behind by a write that failed).
pub fn known_blobs(
    conn: &Connection,
    ids: &[BlobId],
) -> CoreResult<std::collections::HashSet<BlobId>> {
    let mut stmt = conn.prepare_cached("SELECT 1 FROM blobs WHERE id = ?1")?;
    let mut out = std::collections::HashSet::new();
    for id in ids {
        if stmt.exists([id.as_str()])? {
            out.insert(id.clone());
        }
    }
    Ok(out)
}

pub fn get_blob(conn: &Connection, id: &BlobId) -> CoreResult<Option<(String, u64)>> {
    Ok(conn
        .query_row(
            "SELECT mime, size FROM blobs WHERE id = ?1",
            [id.as_str()],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u64)),
        )
        .optional()?)
}

// ----- operations -------------------------------------------------------------------------------

const OPERATION_COLS: &str =
    "id, kind, status, project_id, message, progress, started_at, finished_at, work_dir";

type RawOperation = (
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    i64,
    Option<i64>,
    Option<String>,
);

fn operation_row(r: &Row<'_>) -> rusqlite::Result<RawOperation> {
    Ok((
        r.get(0)?,
        r.get(1)?,
        r.get(2)?,
        r.get(3)?,
        r.get(4)?,
        r.get(5)?,
        r.get(6)?,
        r.get(7)?,
        r.get(8)?,
    ))
}

/// An operation with its working folder (internal: the folder a clone writes into, removed
/// when the operation ends).
fn finish_operation(raw: RawOperation) -> CoreResult<(Operation, Option<String>)> {
    let (id, kind, status, project, message, progress, started_at, finished_at, work_dir) = raw;
    Ok((
        Operation {
            id: OperationId::from(id),
            kind: from_json(&format!("\"{kind}\""))?,
            status: OperationStatus::parse(&status)
                .ok_or_else(|| CoreError::Corrupt(format!("operation status {status}")))?,
            project_id: project.map(ProjectId::from),
            message,
            progress,
            started_at,
            finished_at,
        },
        work_dir,
    ))
}

/// Inserts a new operation, or replaces the public fields of an existing one (the working
/// folder is only set by [`insert_operation`] and cleared by [`clear_operation_work_dir`]).
pub fn upsert_operation(conn: &Connection, op: &Operation) -> CoreResult<()> {
    conn.execute(
        "INSERT INTO operations (id, kind, status, project_id, message, progress, started_at, finished_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         ON CONFLICT(id) DO UPDATE SET status = excluded.status, project_id = excluded.project_id,
           message = excluded.message, progress = excluded.progress, finished_at = excluded.finished_at",
        params![
            op.id.as_str(),
            to_json(&op.kind).trim_matches('"'),
            op.status.as_str(),
            op.project_id.as_ref().map(|p| p.as_str()),
            op.message,
            op.progress,
            op.started_at,
            op.finished_at,
        ],
    )?;
    Ok(())
}

/// Inserts a new operation together with the folder it works in.
pub fn insert_operation(
    conn: &Connection,
    op: &Operation,
    work_dir: Option<&str>,
) -> CoreResult<()> {
    upsert_operation(conn, op)?;
    conn.execute(
        "UPDATE operations SET work_dir = ?2 WHERE id = ?1",
        params![op.id.as_str(), work_dir],
    )?;
    Ok(())
}

/// Forgets the working folder of an operation (it has been removed or became the project).
pub fn clear_operation_work_dir(conn: &Connection, id: &OperationId) -> CoreResult<()> {
    conn.execute(
        "UPDATE operations SET work_dir = NULL WHERE id = ?1",
        [id.as_str()],
    )?;
    Ok(())
}

pub fn get_operation(conn: &Connection, id: &OperationId) -> CoreResult<Option<Operation>> {
    conn.query_row(
        &format!("SELECT {OPERATION_COLS} FROM operations WHERE id = ?1"),
        [id.as_str()],
        operation_row,
    )
    .optional()?
    .map(|raw| finish_operation(raw).map(|(op, _)| op))
    .transpose()
}

/// Forgets the finished operations that ended before `before` (and whose folder is gone).
pub fn delete_finished_operations(conn: &Connection, before: Millis) -> CoreResult<usize> {
    Ok(conn.execute(
        "DELETE FROM operations WHERE finished_at IS NOT NULL AND finished_at < ?1 AND status != 'running' AND work_dir IS NULL",
        [before],
    )?)
}

pub fn list_operations(conn: &Connection, limit: usize) -> CoreResult<Vec<Operation>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {OPERATION_COLS} FROM operations ORDER BY started_at DESC LIMIT ?1"
    ))?;
    let rows = stmt.query_map([limit as i64], operation_row)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(finish_operation(row?)?.0);
    }
    Ok(out)
}

/// Operations still marked running, or whose working folder was not removed yet, with that
/// folder (startup recovery).
pub fn unfinished_operations(conn: &Connection) -> CoreResult<Vec<(Operation, Option<String>)>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {OPERATION_COLS} FROM operations WHERE status = 'running' OR work_dir IS NOT NULL"
    ))?;
    let rows = stmt.query_map([], operation_row)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(finish_operation(row?)?);
    }
    Ok(out)
}

// ----- idempotency ------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct IdemRecord {
    pub method: String,
    pub params_hash: String,
    /// `{"result": …}` or `{"error": …}`.
    pub response: Value,
}

pub fn idem_get(
    conn: &Connection,
    device: &DeviceId,
    client_request_id: &str,
) -> CoreResult<Option<IdemRecord>> {
    conn.query_row(
        "SELECT method, params_hash, response FROM idempotency WHERE device_id = ?1 AND client_request_id = ?2",
        params![device.as_str(), client_request_id],
        |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)),
    )
    .optional()?
    .map(|(method, params_hash, response)| Ok(IdemRecord { method, params_hash, response: from_json(&response)? }))
    .transpose()
}

pub fn idem_put(
    conn: &Connection,
    device: &DeviceId,
    client_request_id: &str,
    method: &str,
    params_hash: &str,
    response: &Value,
    now: Millis,
) -> CoreResult<()> {
    conn.execute(
        "INSERT INTO idempotency (device_id, client_request_id, method, params_hash, response, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![device.as_str(), client_request_id, method, params_hash, response.to_string(), now],
    )?;
    Ok(())
}

pub fn idem_gc(conn: &Connection, before: Millis) -> CoreResult<usize> {
    Ok(conn.execute("DELETE FROM idempotency WHERE created_at < ?1", [before])?)
}

// ----- purging removed threads ------------------------------------------------------------------

/// What purging a thread deleted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PurgeCounts {
    pub turns: usize,
    pub items: usize,
    pub interactions: usize,
    pub queued: usize,
    pub background_tasks: usize,
    pub events: usize,
}

/// Deletes everything stored about thread `id`: its turns, items, interactions, queued
/// inputs, background tasks (and their last end) and blob references (a blob left without
/// any starts its grace period), its event stream and its workspace events other than
/// `thread/removed` (which offline clients still need), and the thread itself.
pub fn purge_thread(
    tx: &rusqlite::Transaction<'_>,
    id: &ThreadId,
    now: Millis,
) -> CoreResult<PurgeCounts> {
    let held: Vec<String> = tx
        .prepare_cached("SELECT DISTINCT blob_id FROM blob_refs WHERE thread_id = ?1")?
        .query_map([id.as_str()], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<_>>()?;
    tx.execute("DELETE FROM blob_refs WHERE thread_id = ?1", [id.as_str()])?;
    for blob in held {
        mark_if_orphaned(tx, &blob, now)?;
    }
    let counts = PurgeCounts {
        items: tx.execute("DELETE FROM items WHERE thread_id = ?1", [id.as_str()])?,
        turns: tx.execute("DELETE FROM turns WHERE thread_id = ?1", [id.as_str()])?,
        interactions: tx.execute(
            "DELETE FROM interactions WHERE thread_id = ?1",
            [id.as_str()],
        )?,
        queued: tx.execute(
            "DELETE FROM queued_inputs WHERE thread_id = ?1",
            [id.as_str()],
        )?,
        background_tasks: tx.execute(
            "DELETE FROM background_tasks WHERE thread_id = ?1",
            [id.as_str()],
        )?,
        events: aas_eventlog::delete_stream(tx, &thread_stream(id))?
            + aas_eventlog::delete_thread_events(
                tx,
                WORKSPACE_STREAM,
                id.as_str(),
                "thread/removed",
            )?,
    };
    tx.execute(
        "DELETE FROM background_last_ended WHERE thread_id = ?1",
        [id.as_str()],
    )?;
    tx.execute("DELETE FROM threads WHERE id = ?1", [id.as_str()])?;
    Ok(counts)
}

// ----- cleanup jobs -----------------------------------------------------------------------------

/// File-system and git work left over from a removal, retried until it succeeds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CleanupKind {
    /// Delete the refs that keep the snapshots of thread `target` reachable in `repo`.
    SnapshotRefs,
    /// Remove the worktree at `target` (created by the daemon) from `repo`.
    Worktree,
    /// Delete the branch `target` of `repo`, created with a worktree for a thread that was
    /// never stored (`thread/create` refused after the worktree existed).
    Branch,
}

impl CleanupKind {
    fn as_str(self) -> &'static str {
        match self {
            CleanupKind::SnapshotRefs => "snapshotRefs",
            CleanupKind::Worktree => "worktree",
            CleanupKind::Branch => "branch",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "snapshotRefs" => Some(CleanupKind::SnapshotRefs),
            "worktree" => Some(CleanupKind::Worktree),
            "branch" => Some(CleanupKind::Branch),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CleanupJob {
    pub id: i64,
    pub kind: CleanupKind,
    pub repo: String,
    pub target: String,
    pub attempts: u32,
}

pub fn insert_cleanup_job(
    conn: &Connection,
    kind: CleanupKind,
    repo: &str,
    target: &str,
    now: Millis,
) -> CoreResult<i64> {
    conn.execute(
        "INSERT INTO cleanup_jobs (kind, repo, target, created_at) VALUES (?1, ?2, ?3, ?4)",
        params![kind.as_str(), repo, target, now],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Pending cleanup jobs, oldest first.
pub fn cleanup_jobs(conn: &Connection) -> CoreResult<Vec<CleanupJob>> {
    let rows: Vec<(i64, String, String, String, i64)> = conn
        .prepare_cached("SELECT id, kind, repo, target, attempts FROM cleanup_jobs ORDER BY id")?
        .query_map([], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    rows.into_iter()
        .map(|(id, kind, repo, target, attempts)| {
            Ok(CleanupJob {
                id,
                kind: CleanupKind::parse(&kind)
                    .ok_or_else(|| CoreError::Corrupt(format!("cleanup job kind {kind}")))?,
                repo,
                target,
                attempts: attempts as u32,
            })
        })
        .collect()
}

pub fn finish_cleanup_job(conn: &Connection, id: i64) -> CoreResult<()> {
    conn.execute("DELETE FROM cleanup_jobs WHERE id = ?1", [id])?;
    Ok(())
}

pub fn fail_cleanup_job(conn: &Connection, id: i64, error: &str) -> CoreResult<()> {
    conn.execute(
        "UPDATE cleanup_jobs SET attempts = attempts + 1, last_error = ?2 WHERE id = ?1",
        params![id, error],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_queued_preview_is_the_first_line_cut_to_its_policy_length() {
        let input = vec![
            InputPart::Text {
                text: "\n  fix the build\nand then".into(),
            },
            InputPart::Mention {
                path: "src/main.rs".into(),
            },
        ];
        assert_eq!(input_preview(&input, 120), "fix the build");
        assert_eq!(input_preview(&input, 3), "fix…");
    }
}
