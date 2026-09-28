//! Collects events during a transaction and appends them to the log in order.

use std::collections::HashMap;

use aas_protocol::events::Event;
use aas_protocol::{Millis, Thread, ThreadId, WORKSPACE_STREAM, thread_stream};
use rusqlite::Transaction;

use crate::error::CoreResult;
use crate::store::{self, ThreadRow};

/// Accumulates events per stream; [`Emitter::flush`] appends them inside the transaction.
pub struct Emitter {
    ts: Millis,
    pending: Vec<(String, Event)>,
    heads: HashMap<String, u64>,
}

impl Emitter {
    pub fn new(ts: Millis) -> Self {
        Self {
            ts,
            pending: Vec::new(),
            heads: HashMap::new(),
        }
    }

    pub fn workspace(&mut self, event: Event) {
        debug_assert!(
            event.is_workspace_event(),
            "{} is not a workspace event",
            event.type_name()
        );
        self.pending.push((WORKSPACE_STREAM.to_owned(), event));
    }

    pub fn thread(&mut self, thread: &ThreadId, event: Event) {
        debug_assert!(
            !event.is_workspace_event(),
            "{} is a workspace event",
            event.type_name()
        );
        self.pending.push((thread_stream(thread), event));
    }

    /// Appends everything pending. May be called several times within one transaction.
    pub fn flush(&mut self, tx: &Transaction<'_>) -> CoreResult<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let mut order: Vec<String> = Vec::new();
        let mut groups: HashMap<String, Vec<Event>> = HashMap::new();
        for (stream, event) in self.pending.drain(..) {
            let group = groups.entry(stream.clone()).or_insert_with(|| {
                order.push(stream.clone());
                Vec::new()
            });
            group.push(event);
        }
        for stream in order {
            let events = groups.remove(&stream).unwrap_or_default();
            let stored = aas_eventlog::append(tx, &stream, self.ts, events)?;
            if let Some(last) = stored.last() {
                self.heads.insert(stream, last.seq);
            }
        }
        Ok(())
    }

    pub fn into_heads(self) -> Vec<(String, u64)> {
        self.heads.into_iter().collect()
    }
}

/// Persists `row` and emits `thread/updated` (thread stream) and `thread/upserted`
/// (workspace). `row.head` is set to the thread stream's head as of this call, i.e. excluding
/// the `thread/updated` event itself.
pub fn thread_changed(
    tx: &Transaction<'_>,
    em: &mut Emitter,
    row: &mut ThreadRow,
) -> CoreResult<Thread> {
    em.flush(tx)?;
    row.head = aas_eventlog::head(tx, &thread_stream(&row.id))?;
    store::update_thread(tx, row)?;
    let view = store::thread_view(tx, row)?;
    em.thread(
        &row.id,
        Event::ThreadUpdated {
            thread: view.clone(),
        },
    );
    em.workspace(Event::ThreadUpserted {
        thread: view.clone(),
    });
    Ok(view)
}
