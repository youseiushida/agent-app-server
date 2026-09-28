//! State shared by the engine and all thread actors.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use aas_eventlog::HeadHub;
use aas_protocol::events::Event;
use aas_protocol::{DeviceId, Harness, Millis};
use aas_supervisor::Supervisor;
use rusqlite::Transaction;
use serde::Serialize;
use serde_json::json;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::blobs::BlobStore;
use crate::capacity::Capacity;
use crate::config::EngineConfig;
use crate::db::Db;
use crate::emit::Emitter;
use crate::error::{CoreError, CoreResult};
use crate::fs::FsApi;
use crate::git::Git;
use crate::operations::OperationRegistry;
use crate::registry::HarnessRegistry;
use crate::store;

/// Idempotency key of a mutating request.
#[derive(Debug, Clone)]
pub struct Idem {
    pub device: DeviceId,
    pub client_request_id: String,
    pub method: &'static str,
    pub params_hash: String,
}

impl Idem {
    /// Stores `{"result": value}` inside the caller's transaction.
    pub fn store_result<T: Serialize>(&self, tx: &Transaction<'_>, value: &T) -> CoreResult<()> {
        let response = json!({ "result": serde_json::to_value(value)? });
        store::idem_put(
            tx,
            &self.device,
            &self.client_request_id,
            self.method,
            &self.params_hash,
            &response,
            store::now_ms(),
        )
    }

    pub fn store_error(
        &self,
        tx: &Transaction<'_>,
        error: &aas_protocol::RpcError,
    ) -> CoreResult<()> {
        let response = json!({ "error": serde_json::to_value(error)? });
        store::idem_put(
            tx,
            &self.device,
            &self.client_request_id,
            self.method,
            &self.params_hash,
            &response,
            store::now_ms(),
        )
    }
}

/// Why the daemon stopped itself: a write that has no client waiting for it (agent events,
/// the end of an operation, …) kept failing, so the event log — the source of truth — could
/// no longer be kept (design.md §6.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageFailure {
    /// What could not be written, and the last error.
    pub message: String,
    pub at: Millis,
}

pub struct Shared {
    pub config: EngineConfig,
    pub db: Db,
    pub hub: HeadHub,
    pub registry: HarnessRegistry,
    pub supervisor: Supervisor,
    pub capacity: Capacity,
    pub blobs: BlobStore,
    pub git: Option<Git>,
    pub fs: FsApi,
    pub running_turns: watch::Sender<usize>,
    /// Background tasks that keep an agent busy (in their harness's live set and not ambient),
    /// over all threads: each thread actor adds what its process holds. A drain waits for
    /// them as for running turns.
    pub running_background: watch::Sender<usize>,
    pub draining: AtomicBool,
    /// Set when Windows ends the session (sign-out, shutdown, reboot) before the engine
    /// shuts down: the turns the shutdown ends are recorded as `systemShutdown` instead of
    /// `daemonShutdown` ([`crate::Engine::shutdown_for_end_session`]).
    pub session_ending: AtomicBool,
    /// Operations (clones) running in this process.
    pub operations: OperationRegistry,
    /// Set once by the fail-stop; never cleared (the daemon exits and is restarted).
    pub storage_failure: watch::Sender<Option<StorageFailure>>,
    /// Held by a maintenance pass and by whoever runs cleanup jobs, so the same job never
    /// runs twice at once.
    pub retention: tokio::sync::Mutex<()>,
    /// Held while a harness's information is read and written to the log, so that the last
    /// `harness/updated` of a harness always carries its latest probe result.
    pub harness_publish: tokio::sync::Mutex<()>,
}

/// Which probe results reach the clients as `harness/updated`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Publish {
    /// Every result (the initial probe, `harness/refresh`).
    Always,
    /// Results that change what clients know (probes the daemon starts by itself).
    IfChanged,
}

impl Shared {
    /// Runs `f` in a write transaction, appends the events it emitted and publishes the new
    /// heads after commit.
    pub async fn tx<T, F>(&self, f: F) -> CoreResult<T>
    where
        T: Send + 'static,
        F: FnOnce(&Transaction<'_>, &mut Emitter) -> CoreResult<T> + Send + 'static,
    {
        let (value, heads) = self
            .db
            .write(move |tx| {
                let mut em = Emitter::new(store::now_ms());
                let value = f(tx, &mut em)?;
                em.flush(tx)?;
                Ok((value, em.into_heads()))
            })
            .await?;
        for (stream, head) in heads {
            self.hub.publish(&stream, head);
        }
        Ok(value)
    }

    /// Like [`tx`](Self::tx), for writes nobody waits for and whose loss would lose data (the
    /// changes of a thread, agent events, the end of an operation): a write that fails for a
    /// storage reason is retried with a doubling wait (`policy.storage_retry_*`). When every
    /// attempt fails the daemon fail-stops ([`fail_stop`](Self::fail_stop)) and the error is
    /// returned. Once the fail-stop has begun, writes are tried once (the daemon is only
    /// winding down; the restart reconciles the state).
    pub async fn tx_durable<T, F>(&self, what: &'static str, f: F) -> CoreResult<T>
    where
        T: Send + 'static,
        F: Fn(&Transaction<'_>, &mut Emitter) -> CoreResult<T> + Send + Sync + 'static,
    {
        let policy = &self.config.policy;
        let f = Arc::new(f);
        let mut attempt = 1u32;
        loop {
            let g = f.clone();
            let error = match self.tx(move |tx, em| g(tx, em)).await {
                Ok(value) => {
                    if attempt > 1 {
                        tracing::info!(what, attempt, "persisting succeeded after a retry");
                    }
                    return Ok(value);
                }
                Err(e) if !e.is_storage_failure() => return Err(e),
                Err(e) => e,
            };
            if self.failed() {
                tracing::warn!(what, error = %error, "not persisted: the daemon is stopping after a storage failure");
                return Err(error);
            }
            if attempt >= policy.storage_retry_attempts {
                self.fail_stop(what, &error);
                return Err(error);
            }
            let wait = policy.storage_retry_backoff(attempt);
            tracing::warn!(what, attempt, error = %error, retry_in = ?wait, "persisting failed; retrying");
            tokio::time::sleep(wait).await;
            attempt += 1;
        }
    }

    /// Starts the fail-stop (once): the engine stops accepting work and stops every agent
    /// process; the daemon tells clients why and exits with a failure so the watchdog
    /// restarts it (the usual recovery then applies).
    pub fn fail_stop(&self, what: &str, error: &CoreError) {
        let failure = StorageFailure {
            message: format!("{what}: {error}"),
            at: store::now_ms(),
        };
        let first = self.storage_failure.send_if_modified(|current| {
            if current.is_some() {
                return false;
            }
            *current = Some(failure.clone());
            true
        });
        if first {
            tracing::error!(fail_stop = true, error = %failure.message, "persisting keeps failing; stopping the daemon");
        }
    }

    /// Whether the fail-stop has begun.
    pub fn failed(&self) -> bool {
        self.storage_failure.borrow().is_some()
    }

    /// Whether new work (turns, queued inputs) may start: not while draining or failing.
    pub fn accepts_work(&self) -> bool {
        !self.is_draining() && !self.failed()
    }

    /// Like [`tx`](Self::tx), also storing the value as the idempotent result of `idem`.
    pub async fn tx_idem<T, F>(&self, idem: Option<Idem>, f: F) -> CoreResult<T>
    where
        T: Serialize + Send + 'static,
        F: FnOnce(&Transaction<'_>, &mut Emitter) -> CoreResult<T> + Send + 'static,
    {
        self.tx(move |tx, em| {
            let value = f(tx, em)?;
            if let Some(idem) = &idem {
                idem.store_result(tx, &value)?;
            }
            Ok(value)
        })
        .await
    }

    pub fn turn_started(&self) {
        self.running_turns.send_modify(|n| *n += 1);
    }

    pub fn turn_finished(&self) {
        self.running_turns.send_modify(|n| *n = n.saturating_sub(1));
    }

    /// A thread's count of background tasks that keep its agent busy went from `before` to
    /// `after`.
    pub fn background_busy_changed(&self, before: usize, after: usize) {
        if before != after {
            self.running_background
                .send_modify(|n| *n = (*n + after).saturating_sub(before));
        }
    }

    pub fn is_draining(&self) -> bool {
        self.draining.load(Ordering::SeqCst)
    }

    /// Probes harness `id` in a task of its own — it completes, and its result is published,
    /// even when the caller stops waiting — unless a probe that started at or after
    /// `fresh_since` has finished (see [`HarnessRegistry::probe`]). The task yields the
    /// harness's protocol view (`None` for an unknown id).
    pub fn probe_harness(
        self: &Arc<Self>,
        id: String,
        fresh_since: Option<Instant>,
        publish: Publish,
    ) -> JoinHandle<Option<Harness>> {
        let sh = self.clone();
        tokio::spawn(async move {
            let probed = sh.registry.probe(&id, fresh_since).await?;
            if publish == Publish::Always || probed.changed {
                sh.publish_harness(&id).await;
            }
            Some(probed.harness)
        })
    }

    /// Writes `harness/updated` with the harness's current information.
    async fn publish_harness(&self, id: &str) {
        let _order = self.harness_publish.lock().await;
        let Some(harness) = self.registry.harness(id) else {
            return;
        };
        let written = self
            .tx_durable("harness information", move |_tx, em| {
                em.workspace(Event::HarnessUpdated {
                    harness: harness.clone(),
                });
                Ok(())
            })
            .await;
        if let Err(e) = written {
            tracing::error!(harness = id, error = %e, "publishing harness information failed");
        }
    }

    /// Called by requests that need harness `id` (design.md §9.4): when its last probe found
    /// it unavailable, probe it again and wait for that probe — at most
    /// `policy.handshake_timeout`, after which the probe goes on by itself — so that a CLI
    /// that was installed or logged in to since is found before the request is refused. A
    /// probe that started less than `policy.harness_probe_min_interval` ago is reused.
    pub async fn recheck_unavailable_harness(self: &Arc<Self>, id: &str) {
        self.registry.wait_ready().await;
        // A stopping daemon refuses the request anyway (`draining`).
        if !self.accepts_work()
            || self.registry.get(id).is_none()
            || self.registry.info(id).is_some_and(|i| i.available)
        {
            return;
        }
        let policy = &self.config.policy;
        // `None` (the monotonic clock started less than the interval ago): any probe is recent.
        let fresh_since = Instant::now().checked_sub(policy.harness_probe_min_interval);
        let probe = self.probe_harness(id.to_owned(), fresh_since, Publish::IfChanged);
        match tokio::time::timeout(policy.handshake_timeout, probe).await {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => tracing::error!(harness = id, error = %e, "the probe task failed"),
            Err(_) => tracing::info!(
                harness = id,
                timeout = ?policy.handshake_timeout,
                "the probe of an unavailable harness is still running; the request is answered with the last result"
            ),
        }
    }

    /// Probes the unavailable harnesses whose retry is due, until the engine is gone
    /// (design.md §9.4). Holds no reference to the engine while it waits.
    pub(crate) async fn retry_unavailable_harnesses(sh: std::sync::Weak<Shared>) {
        let Some(mut completed) = sh.upgrade().map(|s| s.registry.probes_completed()) else {
            return;
        };
        loop {
            let next = {
                let Some(sh) = sh.upgrade() else { return };
                // A draining or failing daemon starts no more turns: nothing needs a harness.
                if sh.failed() || sh.is_draining() {
                    return;
                }
                completed.borrow_and_update();
                let now = Instant::now();
                let schedule = sh.registry.retry_schedule(&sh.config.policy);
                let due: Vec<(String, Instant)> = schedule
                    .iter()
                    .filter(|(_, at)| *at <= now)
                    .cloned()
                    .collect();
                if !due.is_empty() {
                    let probes: Vec<_> = due
                        .into_iter()
                        .map(|(id, at)| {
                            tracing::info!(harness = %id, "probing an unavailable harness again");
                            sh.probe_harness(id, Some(at), Publish::IfChanged)
                        })
                        .collect();
                    drop(sh);
                    for probe in probes {
                        if let Err(e) = probe.await {
                            tracing::error!(error = %e, "a harness probe task failed");
                        }
                    }
                    continue;
                }
                schedule.into_iter().map(|(_, at)| at).min()
            };
            // Woken by the next due time, or by a probe that ended (it may have made a harness
            // unavailable, or changed a due time).
            tokio::select! {
                _ = async {
                    match next {
                        Some(at) => tokio::time::sleep_until(at).await,
                        None => std::future::pending().await,
                    }
                } => {}
                changed = completed.changed() => {
                    if changed.is_err() {
                        return;
                    }
                }
            }
        }
    }
}
