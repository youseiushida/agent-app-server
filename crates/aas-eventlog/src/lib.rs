//! Append-only event log.
//!
//! * Every stream (`workspace`, `thread:<id>`) has a monotonically increasing sequence number
//!   (`streams.head`). Appends happen inside the caller's transaction so domain state and log
//!   never diverge.
//! * After the caller commits it publishes the new heads on the [`HeadHub`]; subscribers wait
//!   on it and then read from their own cursor, so replay and live delivery share one code path.
//! * [`read_batch`] merges consecutive `item/delta` events of the same `(itemId, field)`.
//!   Merging is driven only by what is already stored when the batch is read, never by timers.
//! * Sequence numbers are cursors, not a dense range: compaction deletes old deltas of
//!   completed items (their final content lives in `item/completed`), events whose whole
//!   content a later event of the same entity carries (for example an older
//!   `thread/updated` or `backgroundTask/updated`), and old `native` events; a removed
//!   thread's events are purged.
//! * What compaction and purging need to know about an event is stored with it in explicit
//!   columns derived from its typed content ([`event_keys`]); stored JSON is never searched.

use std::collections::HashMap;

use aas_protocol::ItemId;
use aas_protocol::events::{Event, EventEnvelope};
use aas_protocol::types::Millis;
use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::Value;
use tokio::sync::watch;

/// Errors of the event log.
#[derive(Debug, thiserror::Error)]
pub enum LogError {
    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("stored event {stream}#{seq} is corrupt: {message}")]
    Corrupt {
        stream: String,
        seq: u64,
        message: String,
    },
}

/// Creates the log tables, or brings tables of an earlier layout up to date (adding the key
/// columns and filling them for the events already stored). Idempotent.
pub fn migrate(conn: &Connection) -> Result<(), LogError> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS streams (
             name TEXT PRIMARY KEY NOT NULL,
             head INTEGER NOT NULL
         ) STRICT;
         CREATE TABLE IF NOT EXISTS events (
             stream TEXT NOT NULL,
             seq INTEGER NOT NULL,
             ts INTEGER NOT NULL,
             type TEXT NOT NULL,
             data TEXT NOT NULL,
             item_id TEXT,
             thread_id TEXT,
             snapshot_key TEXT,
             supersedable INTEGER NOT NULL DEFAULT 0,
             PRIMARY KEY (stream, seq)
         ) STRICT, WITHOUT ROWID;",
    )?;
    let columns: Vec<String> = conn
        .prepare("PRAGMA table_info(events)")?
        .query_map([], |r| r.get::<_, String>(1))?
        .collect::<rusqlite::Result<_>>()?;
    if !columns.iter().any(|c| c == "snapshot_key") {
        conn.execute_batch(
            "ALTER TABLE events ADD COLUMN thread_id TEXT;
             ALTER TABLE events ADD COLUMN snapshot_key TEXT;
             ALTER TABLE events ADD COLUMN supersedable INTEGER NOT NULL DEFAULT 0;",
        )?;
        backfill_keys(conn)?;
    }
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS events_item ON events(item_id) WHERE item_id IS NOT NULL;
         CREATE INDEX IF NOT EXISTS events_thread ON events(stream, thread_id) WHERE thread_id IS NOT NULL;
         CREATE INDEX IF NOT EXISTS events_snapshot ON events(stream, snapshot_key, seq) WHERE snapshot_key IS NOT NULL;
         CREATE INDEX IF NOT EXISTS events_supersedable ON events(ts) WHERE supersedable = 1;
         CREATE INDEX IF NOT EXISTS events_type_ts ON events(type, ts);",
    )?;
    Ok(())
}

/// Fills the key columns of events stored before they existed (decoding each keyed event).
fn backfill_keys(conn: &Connection) -> Result<(), LogError> {
    let rows: Vec<(String, u64, String, String)> = conn
        .prepare("SELECT stream, seq, type, data FROM events")?
        .query_map([], |r| {
            Ok((r.get(0)?, r.get::<_, i64>(1)? as u64, r.get(2)?, r.get(3)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let mut update =
        conn.prepare("UPDATE events SET thread_id = ?3, snapshot_key = ?4, supersedable = ?5 WHERE stream = ?1 AND seq = ?2")?;
    for (stream, seq, type_name, data) in rows {
        let keys = event_keys(&decode(&stream, seq, &type_name, &data)?);
        if keys.thread_id.is_some() || keys.snapshot_key.is_some() {
            update.execute(params![
                stream,
                seq as i64,
                keys.thread_id,
                keys.snapshot_key,
                keys.supersedable
            ])?;
        }
    }
    Ok(())
}

/// What the log stores about an event besides its content, derived from its typed fields.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EventKeys {
    /// The item the event is about (compaction of the deltas of completed items).
    pub item_id: Option<String>,
    /// The thread a workspace event is about (purged with the thread).
    pub thread_id: Option<String>,
    /// The entity whose state the event carries in full (`thread:<id>`, `item:<id>`, …).
    pub snapshot_key: Option<String>,
    /// Whether a later event with the same `snapshot_key` in the same stream carries all of
    /// this event's content, so this one may be deleted once it is old enough. Events that
    /// mark an end (`turn/completed`, `item/completed`, `thread/removed`,
    /// `interaction/closed`) are never deleted by this rule.
    pub supersedable: bool,
}

/// The keys of `event` (see [`EventKeys`]). Every event type is listed, so a new one needs a
/// decision here.
pub fn event_keys(event: &Event) -> EventKeys {
    let keyed = |key: String, supersedable: bool| (Some(key), supersedable);
    let (snapshot_key, supersedable) = match event {
        Event::ProjectUpserted { project } => keyed(format!("project:{}", project.id), true),
        // A removed project can be opened again (same id): the later upsert supersedes it.
        Event::ProjectRemoved { project_id } => keyed(format!("project:{project_id}"), true),
        Event::ThreadUpserted { thread } => keyed(format!("thread:{}", thread.id), true),
        Event::ThreadRemoved { thread_id } => keyed(format!("thread:{thread_id}"), false),
        Event::InteractionPending { interaction } => {
            keyed(format!("interaction:{}", interaction.id), true)
        }
        Event::InteractionClosed { interaction_id, .. } => {
            keyed(format!("interaction:{interaction_id}"), false)
        }
        Event::HarnessUpdated { harness } => keyed(format!("harness:{}", harness.id), true),
        Event::OperationUpdated { operation } => keyed(format!("operation:{}", operation.id), true),
        Event::ThreadUpdated { thread } => keyed(format!("thread:{}", thread.id), true),
        Event::TurnUsageUpdated { turn_id, .. } => keyed(format!("turn:{turn_id}"), true),
        Event::TurnCompleted { turn } => keyed(format!("turn:{}", turn.id), false),
        Event::ItemUpdated { item } => keyed(format!("item:{}", item.id), true),
        Event::ItemCompleted { item } => keyed(format!("item:{}", item.id), false),
        Event::QueueUpdated { .. } => keyed("queue".to_owned(), true),
        Event::CommandsChanged {} => keyed("commands".to_owned(), true),
        // Every update carries the whole task, a restart (a new run) included: a later one
        // replaces it. The last update of a task stays, whatever its state.
        Event::BackgroundTaskUpdated { task } => keyed(format!("background:{}", task.id), true),
        // Each of these adds something no later event repeats.
        Event::TurnStarted { .. }
        | Event::TurnDiffUpdated { .. }
        | Event::ItemStarted { .. }
        | Event::ItemDelta { .. }
        | Event::InteractionRequested { .. }
        | Event::InteractionResolved { .. }
        | Event::InteractionExpired { .. }
        | Event::Native { .. } => (None, false),
    };
    let thread_id = match event {
        Event::ThreadUpserted { thread } => Some(thread.id.to_string()),
        Event::ThreadRemoved { thread_id } | Event::InteractionClosed { thread_id, .. } => {
            Some(thread_id.to_string())
        }
        Event::InteractionPending { interaction } => Some(interaction.thread_id.to_string()),
        _ => None,
    };
    EventKeys {
        item_id: event.item_id().map(|i| i.as_str().to_owned()),
        thread_id,
        snapshot_key,
        supersedable,
    }
}

/// Current head of a stream (0 when the stream has no events yet).
pub fn head(conn: &Connection, stream: &str) -> rusqlite::Result<u64> {
    Ok(conn
        .query_row("SELECT head FROM streams WHERE name = ?1", [stream], |r| {
            r.get::<_, i64>(0)
        })
        .optional()?
        .unwrap_or(0) as u64)
}

/// Heads of many streams at once.
pub fn heads(conn: &Connection, streams: &[String]) -> rusqlite::Result<HashMap<String, u64>> {
    let mut out = HashMap::with_capacity(streams.len());
    let mut stmt = conn.prepare_cached("SELECT head FROM streams WHERE name = ?1")?;
    for s in streams {
        let h = stmt
            .query_row([s], |r| r.get::<_, i64>(0))
            .optional()?
            .unwrap_or(0) as u64;
        out.insert(s.clone(), h);
    }
    Ok(out)
}

/// Appends `events` to `stream` inside `tx`. Returns the stored envelopes (with their
/// sequence numbers). The caller publishes the new head on the [`HeadHub`] after commit.
pub fn append(
    tx: &Transaction<'_>,
    stream: &str,
    ts: Millis,
    events: Vec<Event>,
) -> Result<Vec<EventEnvelope>, LogError> {
    if events.is_empty() {
        return Ok(Vec::new());
    }
    let mut seq = head(tx, stream)?;
    let mut out = Vec::with_capacity(events.len());
    {
        let mut insert = tx.prepare_cached(
            "INSERT INTO events (stream, seq, ts, type, data, item_id, thread_id, snapshot_key, supersedable)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        )?;
        for event in events {
            seq += 1;
            let (type_name, data) = split_event(&event);
            let keys = event_keys(&event);
            insert.execute(params![
                stream,
                seq as i64,
                ts,
                type_name,
                data.to_string(),
                keys.item_id,
                keys.thread_id,
                keys.snapshot_key,
                keys.supersedable
            ])?;
            out.push(EventEnvelope {
                seq,
                seq_from: None,
                ts,
                event,
            });
        }
    }
    tx.execute(
        "INSERT INTO streams (name, head) VALUES (?1, ?2)
         ON CONFLICT(name) DO UPDATE SET head = excluded.head",
        params![stream, seq as i64],
    )?;
    Ok(out)
}

fn split_event(event: &Event) -> (&'static str, Value) {
    let mut v = serde_json::to_value(event).expect("events always serialize");
    let data = v
        .get_mut("data")
        .map(Value::take)
        .unwrap_or(Value::Object(Default::default()));
    (event.type_name(), data)
}

/// Limits of one [`read_batch`].
#[derive(Debug, Clone, Copy)]
pub struct BatchLimits {
    pub max_events: usize,
    pub max_bytes: usize,
}

/// Result of [`read_batch`].
#[derive(Debug, Clone, PartialEq)]
pub struct Batch {
    pub events: Vec<EventEnvelope>,
    /// Highest sequence number covered by this batch (the next cursor). Equals `after` when
    /// nothing was read.
    pub last_seq: u64,
    /// Head of the stream at the time of reading.
    pub head: u64,
}

/// Reads events after `after`, merging consecutive deltas of the same item field.
pub fn read_batch(
    conn: &Connection,
    stream: &str,
    after: u64,
    limits: BatchLimits,
) -> Result<Batch, LogError> {
    let head = head(conn, stream)?;
    let mut stmt = conn.prepare_cached(
        "SELECT seq, ts, type, data FROM events WHERE stream = ?1 AND seq > ?2 ORDER BY seq LIMIT ?3",
    )?;
    let rows = stmt.query_map(
        params![stream, after as i64, limits.max_events.max(1) as i64],
        |r| {
            Ok((
                r.get::<_, i64>(0)? as u64,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
            ))
        },
    )?;

    let mut events: Vec<EventEnvelope> = Vec::new();
    let mut bytes = 0usize;
    let mut last_seq = after;
    for row in rows {
        let (seq, ts, type_name, data) = row?;
        if !events.is_empty() && bytes + data.len() > limits.max_bytes {
            break;
        }
        bytes += data.len();
        last_seq = seq;
        let event = decode(stream, seq, &type_name, &data)?;
        if let Some(prev) = events.last_mut()
            && let (
                Event::ItemDelta {
                    item_id: prev_item,
                    field: prev_field,
                    text: prev_text,
                },
                Event::ItemDelta {
                    item_id,
                    field,
                    text,
                },
            ) = (&mut prev.event, &event)
            && prev_item == item_id
            && prev_field == field
        {
            prev_text.push_str(text);
            prev.seq_from.get_or_insert(prev.seq);
            prev.seq = seq;
            prev.ts = ts;
            continue;
        }
        events.push(EventEnvelope {
            seq,
            seq_from: None,
            ts,
            event,
        });
    }
    Ok(Batch {
        events,
        last_seq,
        head,
    })
}

fn decode(stream: &str, seq: u64, type_name: &str, data: &str) -> Result<Event, LogError> {
    let data: Value = serde_json::from_str(data).map_err(|e| LogError::Corrupt {
        stream: stream.to_owned(),
        seq,
        message: e.to_string(),
    })?;
    serde_json::from_value(serde_json::json!({ "type": type_name, "data": data })).map_err(|e| {
        LogError::Corrupt {
            stream: stream.to_owned(),
            seq,
            message: e.to_string(),
        }
    })
}

/// Deletes `item/delta` events of the given completed items that are older than `before`.
/// Returns the number of deleted rows.
pub fn compact_deltas(
    tx: &Transaction<'_>,
    before: Millis,
    completed_items: &[ItemId],
) -> rusqlite::Result<usize> {
    let mut stmt = tx.prepare_cached(
        "DELETE FROM events WHERE item_id = ?1 AND type = 'item/delta' AND ts < ?2",
    )?;
    let mut total = 0;
    for item in completed_items {
        total += stmt.execute(params![item.as_str(), before])?;
    }
    Ok(total)
}

/// Delta events that are eligible for compaction: deltas older than `before` whose item has
/// a later `item/completed` event. Returned as item ids (at most `limit`).
pub fn compactable_items(
    conn: &Connection,
    before: Millis,
    limit: usize,
) -> rusqlite::Result<Vec<ItemId>> {
    let mut stmt = conn.prepare_cached(
        "SELECT DISTINCT d.item_id FROM events d
         WHERE d.type = 'item/delta' AND d.ts < ?1 AND d.item_id IS NOT NULL
           AND EXISTS (SELECT 1 FROM events c WHERE c.item_id = d.item_id AND c.type = 'item/completed')
         LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![before, limit as i64], |r| r.get::<_, String>(0))?;
    rows.map(|r| r.map(ItemId::from)).collect()
}

fn delete_rows(tx: &Transaction<'_>, rows: &[(String, i64)]) -> rusqlite::Result<usize> {
    let mut delete = tx.prepare_cached("DELETE FROM events WHERE stream = ?1 AND seq = ?2")?;
    let mut total = 0;
    for (stream, seq) in rows {
        total += delete.execute(params![stream, seq])?;
    }
    Ok(total)
}

/// Deletes up to `limit` events older than `before` whose content a later event of the same
/// entity in the same stream carries in full ([`EventKeys::supersedable`]). Returns the
/// number deleted; the caller repeats while it equals `limit`.
pub fn compact_superseded(
    tx: &Transaction<'_>,
    before: Millis,
    limit: usize,
) -> rusqlite::Result<usize> {
    let rows: Vec<(String, i64)> = tx
        .prepare_cached(
            "SELECT e.stream, e.seq FROM events e
             WHERE e.supersedable = 1 AND e.ts < ?1
               AND EXISTS (SELECT 1 FROM events l
                           WHERE l.stream = e.stream AND l.snapshot_key = e.snapshot_key AND l.seq > e.seq)
             LIMIT ?2",
        )?
        .query_map(params![before, limit as i64], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    delete_rows(tx, &rows)
}

/// Deletes up to `limit` events of type `type_name` older than `before` (used for `native`
/// events, which no state is built from). Returns the number deleted.
pub fn compact_type(
    tx: &Transaction<'_>,
    type_name: &str,
    before: Millis,
    limit: usize,
) -> rusqlite::Result<usize> {
    let rows: Vec<(String, i64)> = tx
        .prepare_cached("SELECT stream, seq FROM events WHERE type = ?1 AND ts < ?2 LIMIT ?3")?
        .query_map(params![type_name, before, limit as i64], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    delete_rows(tx, &rows)
}

/// Deletes a whole stream: its events and its head (a removed thread's stream). Returns the
/// number of events deleted.
pub fn delete_stream(tx: &Transaction<'_>, stream: &str) -> rusqlite::Result<usize> {
    let deleted = tx.execute("DELETE FROM events WHERE stream = ?1", [stream])?;
    tx.execute("DELETE FROM streams WHERE name = ?1", [stream])?;
    Ok(deleted)
}

/// Deletes the events of `stream` about thread `thread_id` ([`EventKeys::thread_id`]),
/// except those of type `keep_type` (the removal notice clients still need). Returns the
/// number deleted.
pub fn delete_thread_events(
    tx: &Transaction<'_>,
    stream: &str,
    thread_id: &str,
    keep_type: &str,
) -> rusqlite::Result<usize> {
    tx.execute(
        "DELETE FROM events WHERE stream = ?1 AND thread_id = ?2 AND type != ?3",
        params![stream, thread_id, keep_type],
    )
}

/// Wakes subscribers when a stream's head moves.
#[derive(Default)]
pub struct HeadHub {
    senders: Mutex<HashMap<String, watch::Sender<u64>>>,
}

impl HeadHub {
    pub fn new() -> Self {
        Self::default()
    }

    /// Publishes a new head (call after the transaction that produced it has committed).
    pub fn publish(&self, stream: &str, head: u64) {
        let mut senders = self.senders.lock();
        match senders.get(stream) {
            Some(tx) => {
                tx.send_if_modified(|cur| {
                    if head > *cur {
                        *cur = head;
                        true
                    } else {
                        false
                    }
                });
            }
            None => {
                let (tx, _) = watch::channel(head);
                senders.insert(stream.to_owned(), tx);
            }
        }
    }

    /// Subscribes to head changes of `stream`. The value only ever holds heads published after
    /// a commit (0 when nothing was published since the stream was last pruned); subscribers
    /// cannot change it, so one subscriber's cursor can never wake or stall another.
    ///
    /// Use: subscribe, then read the log from your cursor; when that read found nothing new,
    /// wait for a change (a commit after the read publishes after it, so it is never missed).
    pub fn subscribe(&self, stream: &str) -> watch::Receiver<u64> {
        let mut senders = self.senders.lock();
        senders
            .entry(stream.to_owned())
            .or_insert_with(|| watch::channel(0).0)
            .subscribe()
    }

    /// Drops the sender of a stream nobody listens to any more (keeps the map small).
    pub fn prune(&self) {
        self.senders.lock().retain(|_, tx| tx.receiver_count() > 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aas_protocol::examples;
    use aas_protocol::types::DeltaField;
    use pretty_assertions::assert_eq;

    fn db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        conn
    }

    fn delta(item: &str, field: DeltaField, text: &str) -> Event {
        Event::ItemDelta {
            item_id: ItemId::from(item),
            field,
            text: text.into(),
        }
    }

    #[test]
    fn append_assigns_increasing_sequence_numbers() {
        let mut conn = db();
        let tx = conn.transaction().unwrap();
        let a = append(
            &tx,
            "s",
            10,
            vec![Event::CommandsChanged {}, Event::CommandsChanged {}],
        )
        .unwrap();
        let b = append(&tx, "s", 11, vec![Event::CommandsChanged {}]).unwrap();
        let other = append(&tx, "t", 11, vec![Event::CommandsChanged {}]).unwrap();
        tx.commit().unwrap();
        assert_eq!(a.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![1, 2]);
        assert_eq!(b[0].seq, 3);
        assert_eq!(other[0].seq, 1);
        assert_eq!(head(&conn, "s").unwrap(), 3);
        assert_eq!(head(&conn, "missing").unwrap(), 0);
    }

    #[test]
    fn read_round_trips_every_event_type() {
        let mut conn = db();
        let events: Vec<Event> = examples::events().into_iter().map(|e| e.event).collect();
        let tx = conn.transaction().unwrap();
        append(&tx, "s", 5, events.clone()).unwrap();
        tx.commit().unwrap();
        let batch = read_batch(
            &conn,
            "s",
            0,
            BatchLimits {
                max_events: 1000,
                max_bytes: usize::MAX,
            },
        )
        .unwrap();
        assert_eq!(
            batch
                .events
                .iter()
                .map(|e| e.event.clone())
                .collect::<Vec<_>>(),
            events
        );
        assert_eq!(batch.last_seq, events.len() as u64);
        assert_eq!(batch.head, events.len() as u64);
    }

    #[test]
    fn consecutive_deltas_of_the_same_field_are_merged() {
        let mut conn = db();
        let tx = conn.transaction().unwrap();
        append(
            &tx,
            "s",
            1,
            vec![
                delta("itm_a", DeltaField::Text, "Hel"),
                delta("itm_a", DeltaField::Text, "lo"),
                delta("itm_a", DeltaField::Output, "x"),
                delta("itm_b", DeltaField::Text, "1"),
                delta("itm_b", DeltaField::Text, "2"),
                Event::CommandsChanged {},
                delta("itm_b", DeltaField::Text, "3"),
            ],
        )
        .unwrap();
        tx.commit().unwrap();
        let batch = read_batch(
            &conn,
            "s",
            0,
            BatchLimits {
                max_events: 100,
                max_bytes: usize::MAX,
            },
        )
        .unwrap();
        let summary: Vec<(u64, Option<u64>, Event)> = batch
            .events
            .into_iter()
            .map(|e| (e.seq, e.seq_from, e.event))
            .collect();
        assert_eq!(
            summary,
            vec![
                (2, Some(1), delta("itm_a", DeltaField::Text, "Hello")),
                (3, None, delta("itm_a", DeltaField::Output, "x")),
                (5, Some(4), delta("itm_b", DeltaField::Text, "12")),
                (6, None, Event::CommandsChanged {}),
                (7, None, delta("itm_b", DeltaField::Text, "3")),
            ]
        );
        assert_eq!(batch.last_seq, 7);
    }

    #[test]
    fn batches_respect_limits_and_resume_from_cursor() {
        let mut conn = db();
        let tx = conn.transaction().unwrap();
        append(
            &tx,
            "s",
            1,
            (0..10).map(|_| Event::CommandsChanged {}).collect(),
        )
        .unwrap();
        tx.commit().unwrap();
        let limits = BatchLimits {
            max_events: 4,
            max_bytes: usize::MAX,
        };
        let first = read_batch(&conn, "s", 0, limits).unwrap();
        assert_eq!(first.events.len(), 4);
        assert_eq!(first.last_seq, 4);
        let second = read_batch(&conn, "s", first.last_seq, limits).unwrap();
        assert_eq!(second.events.first().unwrap().seq, 5);
        let tiny = read_batch(
            &conn,
            "s",
            0,
            BatchLimits {
                max_events: 100,
                max_bytes: 1,
            },
        )
        .unwrap();
        assert_eq!(
            tiny.events.len(),
            1,
            "at least one event is always returned"
        );
        let empty = read_batch(&conn, "s", 10, limits).unwrap();
        assert!(empty.events.is_empty());
        assert_eq!(empty.last_seq, 10);
    }

    #[test]
    fn compaction_removes_old_deltas_of_completed_items_only() {
        let mut conn = db();
        let mut completed = examples::items()[6].clone();
        completed.status = aas_protocol::ItemStatus::Completed;
        let done_id = completed.id.clone();
        let tx = conn.transaction().unwrap();
        append(
            &tx,
            "s",
            100,
            vec![
                delta(done_id.as_str(), DeltaField::Text, "a"),
                delta("itm_open", DeltaField::Text, "b"),
                Event::ItemCompleted { item: completed },
            ],
        )
        .unwrap();
        tx.commit().unwrap();
        let items = compactable_items(&conn, 200, 100).unwrap();
        assert_eq!(items, vec![done_id.clone()]);
        let tx = conn.transaction().unwrap();
        assert_eq!(compact_deltas(&tx, 200, &items).unwrap(), 1);
        tx.commit().unwrap();
        let batch = read_batch(
            &conn,
            "s",
            0,
            BatchLimits {
                max_events: 100,
                max_bytes: usize::MAX,
            },
        )
        .unwrap();
        let seqs: Vec<u64> = batch.events.iter().map(|e| e.seq).collect();
        assert_eq!(seqs, vec![2, 3], "cursor semantics survive gaps");
    }

    fn all(conn: &Connection, stream: &str) -> Vec<(u64, Event)> {
        read_batch(
            conn,
            stream,
            0,
            BatchLimits {
                max_events: 10_000,
                max_bytes: usize::MAX,
            },
        )
        .unwrap()
        .events
        .into_iter()
        .map(|e| (e.seq, e.event))
        .collect()
    }

    fn thread(id: &str, title: &str) -> aas_protocol::Thread {
        aas_protocol::Thread {
            id: aas_protocol::ThreadId::from(id),
            title: title.into(),
            ..examples::thread()
        }
    }

    #[test]
    fn every_event_type_has_a_deliberate_key() {
        for env in examples::events() {
            let keys = event_keys(&env.event);
            if keys.supersedable {
                assert!(
                    keys.snapshot_key.is_some(),
                    "{} is supersedable without a key",
                    env.event.type_name()
                );
            }
            if env.event.is_workspace_event() {
                let about_thread = matches!(
                    env.event,
                    Event::ThreadUpserted { .. }
                        | Event::ThreadRemoved { .. }
                        | Event::InteractionPending { .. }
                        | Event::InteractionClosed { .. }
                );
                assert_eq!(
                    keys.thread_id.is_some(),
                    about_thread,
                    "{}",
                    env.event.type_name()
                );
            }
        }
        // Ends are never superseded.
        for name in [
            "turn/completed",
            "item/completed",
            "thread/removed",
            "interaction/closed",
        ] {
            let env = examples::events()
                .into_iter()
                .find(|e| e.event.type_name() == name)
                .unwrap();
            assert!(!event_keys(&env.event).supersedable, "{name}");
        }
    }

    #[test]
    fn superseded_snapshots_are_compacted_and_the_latest_state_survives() {
        let mut conn = db();
        let tx = conn.transaction().unwrap();
        append(
            &tx,
            "s",
            10,
            vec![
                Event::ThreadUpdated {
                    thread: thread("thr_a", "one"),
                },
                Event::ThreadUpdated {
                    thread: thread("thr_b", "b"),
                },
                Event::CommandsChanged {},
                Event::ThreadUpdated {
                    thread: thread("thr_a", "two"),
                },
                Event::CommandsChanged {},
            ],
        )
        .unwrap();
        append(
            &tx,
            "t",
            10,
            vec![Event::ThreadUpdated {
                thread: thread("thr_a", "other stream"),
            }],
        )
        .unwrap();
        append(
            &tx,
            "s",
            50,
            vec![Event::ThreadUpdated {
                thread: thread("thr_a", "three"),
            }],
        )
        .unwrap();
        tx.commit().unwrap();

        // Only events older than the cutoff go, and only when a later one of the same entity
        // exists in the same stream.
        let tx = conn.transaction().unwrap();
        assert_eq!(compact_superseded(&tx, 20, 100).unwrap(), 3);
        tx.commit().unwrap();
        let left: Vec<u64> = all(&conn, "s").into_iter().map(|(seq, _)| seq).collect();
        assert_eq!(
            left,
            vec![2, 5, 6],
            "thr_b's only snapshot, the last commands/changed and thr_a's latest stay"
        );
        assert_eq!(all(&conn, "t").len(), 1, "another stream is independent");
        match &all(&conn, "s").last().unwrap().1 {
            Event::ThreadUpdated { thread } => assert_eq!(thread.title, "three"),
            other => panic!("unexpected {other:?}"),
        }

        // Batches: a limit smaller than the work is honoured and the rest follows.
        let tx = conn.transaction().unwrap();
        append(
            &tx,
            "u",
            1,
            (0..5).map(|_| Event::CommandsChanged {}).collect(),
        )
        .unwrap();
        assert_eq!(compact_superseded(&tx, 20, 3).unwrap(), 3);
        assert_eq!(compact_superseded(&tx, 20, 3).unwrap(), 1);
        assert_eq!(compact_superseded(&tx, 20, 3).unwrap(), 0);
        tx.commit().unwrap();
    }

    #[test]
    fn superseded_background_task_updates_are_compacted() {
        let mut conn = db();
        let running = examples::background_task();
        let progressed = aas_protocol::BackgroundTask {
            progress: None,
            ..running.clone()
        };
        let ended = aas_protocol::BackgroundTask {
            status: aas_protocol::BackgroundTaskStatus::Completed,
            ended_at: Some(9),
            ..running.clone()
        };
        let other = examples::finished_shell();
        let tx = conn.transaction().unwrap();
        append(
            &tx,
            "s",
            1,
            [&running, &other, &progressed, &ended]
                .into_iter()
                .map(|task| Event::BackgroundTaskUpdated { task: task.clone() })
                .collect(),
        )
        .unwrap();
        tx.commit().unwrap();
        let tx = conn.transaction().unwrap();
        assert_eq!(compact_superseded(&tx, 100, 100).unwrap(), 2);
        tx.commit().unwrap();
        let left: Vec<Event> = all(&conn, "s").into_iter().map(|(_, e)| e).collect();
        assert_eq!(
            left,
            vec![
                Event::BackgroundTaskUpdated { task: other },
                Event::BackgroundTaskUpdated { task: ended },
            ],
            "the latest state of each task stays"
        );
    }

    #[test]
    fn ends_are_kept_even_when_followed_by_the_same_entity() {
        let mut conn = db();
        let mut item = examples::items()[6].clone();
        item.status = aas_protocol::ItemStatus::InProgress;
        let mut done = item.clone();
        done.status = aas_protocol::ItemStatus::Completed;
        let tx = conn.transaction().unwrap();
        append(
            &tx,
            "s",
            1,
            vec![
                Event::ItemStarted { item: item.clone() },
                Event::ItemUpdated { item: item.clone() },
                Event::ItemCompleted { item: done.clone() },
                Event::ThreadRemoved {
                    thread_id: aas_protocol::ThreadId::from("thr_x"),
                },
            ],
        )
        .unwrap();
        tx.commit().unwrap();
        let tx = conn.transaction().unwrap();
        assert_eq!(
            compact_superseded(&tx, 100, 100).unwrap(),
            1,
            "only item/updated is superseded"
        );
        tx.commit().unwrap();
        let types: Vec<&str> = all(&conn, "s").iter().map(|(_, e)| e.type_name()).collect();
        assert_eq!(
            types,
            vec!["item/started", "item/completed", "thread/removed"]
        );
    }

    #[test]
    fn native_events_expire_and_removed_threads_are_purged() {
        let mut conn = db();
        let native = |n: i64| Event::Native {
            harness_id: "fake".into(),
            payload: serde_json::json!({ "n": n }),
        };
        let tx = conn.transaction().unwrap();
        append(
            &tx,
            "thread:thr_a",
            1,
            vec![native(1), Event::CommandsChanged {}, native(2)],
        )
        .unwrap();
        append(&tx, "thread:thr_a", 100, vec![native(3)]).unwrap();
        append(
            &tx,
            "workspace",
            1,
            vec![
                Event::ThreadUpserted {
                    thread: thread("thr_a", "a"),
                },
                Event::ThreadUpserted {
                    thread: thread("thr_b", "b"),
                },
                Event::ThreadRemoved {
                    thread_id: aas_protocol::ThreadId::from("thr_a"),
                },
            ],
        )
        .unwrap();
        tx.commit().unwrap();

        let tx = conn.transaction().unwrap();
        assert_eq!(compact_type(&tx, "native", 50, 100).unwrap(), 2);
        tx.commit().unwrap();
        assert_eq!(
            all(&conn, "thread:thr_a").len(),
            2,
            "the recent native event and commands/changed stay"
        );

        let tx = conn.transaction().unwrap();
        assert_eq!(delete_stream(&tx, "thread:thr_a").unwrap(), 2);
        assert_eq!(
            delete_thread_events(&tx, "workspace", "thr_a", "thread/removed").unwrap(),
            1
        );
        tx.commit().unwrap();
        assert!(all(&conn, "thread:thr_a").is_empty());
        assert_eq!(
            head(&conn, "thread:thr_a").unwrap(),
            0,
            "the stream is gone"
        );
        let ws: Vec<String> = all(&conn, "workspace")
            .iter()
            .map(|(_, e)| e.type_name().to_owned())
            .collect();
        assert_eq!(
            ws,
            vec!["thread/upserted", "thread/removed"],
            "thr_b's summary and the removal notice stay"
        );
    }

    #[test]
    fn a_log_of_the_earlier_layout_gets_its_keys() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE streams (name TEXT PRIMARY KEY NOT NULL, head INTEGER NOT NULL) STRICT;
             CREATE TABLE events (stream TEXT NOT NULL, seq INTEGER NOT NULL, ts INTEGER NOT NULL, type TEXT NOT NULL,
                                  data TEXT NOT NULL, item_id TEXT, PRIMARY KEY (stream, seq)) STRICT, WITHOUT ROWID;",
        )
        .unwrap();
        for (seq, event) in [
            Event::ThreadUpserted {
                thread: thread("thr_a", "one"),
            },
            Event::ThreadUpserted {
                thread: thread("thr_a", "two"),
            },
        ]
        .into_iter()
        .enumerate()
        {
            let (type_name, data) = split_event(&event);
            conn.execute(
                "INSERT INTO events (stream, seq, ts, type, data) VALUES ('workspace', ?1, 1, ?2, ?3)",
                params![seq as i64 + 1, type_name, data.to_string()],
            )
            .unwrap();
        }
        migrate(&conn).unwrap();
        migrate(&conn).unwrap();
        let tx = conn.transaction().unwrap();
        assert_eq!(
            compact_superseded(&tx, 10, 100).unwrap(),
            1,
            "the backfilled keys are used"
        );
        assert_eq!(
            delete_thread_events(&tx, "workspace", "thr_a", "thread/removed").unwrap(),
            1
        );
        tx.commit().unwrap();
    }

    #[tokio::test]
    async fn head_hub_wakes_subscribers() {
        let hub = HeadHub::new();
        hub.publish("s", 3);
        let mut rx = hub.subscribe("s");
        assert_eq!(*rx.borrow_and_update(), 3);
        hub.publish("s", 2); // stale publish is ignored
        assert!(!rx.has_changed().unwrap());
        hub.publish("s", 5);
        rx.changed().await.unwrap();
        assert_eq!(*rx.borrow_and_update(), 5);
        let late = hub.subscribe("s");
        assert_eq!(*late.borrow(), 5);
        drop(rx);
        drop(late);
        hub.prune();
        assert!(hub.senders.lock().is_empty());
    }

    #[tokio::test]
    async fn subscribers_cannot_move_the_published_head() {
        let hub = HeadHub::new();
        hub.publish("s", 50);
        let mut other = hub.subscribe("s");
        assert_eq!(*other.borrow_and_update(), 50);
        // A client whose cursor is far ahead of the head subscribes: nothing changes for
        // anyone, and later heads are still published.
        let _ahead = hub.subscribe("s");
        assert!(!other.has_changed().unwrap(), "no spurious wake-up");
        assert_eq!(*other.borrow(), 50);
        hub.publish("s", 51);
        other.changed().await.unwrap();
        assert_eq!(*other.borrow_and_update(), 51);
        // A stream nobody published to yet starts at 0.
        assert_eq!(*hub.subscribe("fresh").borrow(), 0);
    }
}
