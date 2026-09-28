//! The engine: request dispatch, idempotency, recovery, pairing, blobs and stream access.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use aas_eventlog::{Batch, HeadHub};
use aas_harness::{CommandContext, HarnessInfo};
use aas_protocol::events::Event;
use aas_protocol::http::{BlobUploadResponse, PairResponse, PairServerInfo};
use aas_protocol::methods::*;
use aas_protocol::notifications::ShutdownReason;
use aas_protocol::*;
use aas_supervisor::Supervisor;
use parking_lot::Mutex;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::{broadcast, oneshot, watch};

use crate::actor::{self, ActorHandle, Msg, Reply};
use crate::auth::{self, RateLimiter};
use crate::blobs::BlobStore;
use crate::capacity::Capacity;
use crate::config::{EngineConfig, Policy, canonical_roots, path_key};
use crate::emit::{Emitter, thread_changed};
use crate::error::{CoreError, CoreResult, invalid_params, invalid_state, not_found, rpc};
use crate::fs::{FsApi, validate_name};
use crate::git::{Git, quick_info};
use crate::operations::{self, CloneJob, OperationRegistry};
use crate::registry::HarnessRegistry;
use crate::registry::unavailable_error;
use crate::retention::{self, MaintenanceReport};
use crate::shared::{Idem, Publish, Shared, StorageFailure};
use crate::store::{self, CleanupKind, ThreadRow, TurnRow, now_ms};

/// Who is calling.
#[derive(Debug, Clone)]
pub struct RequestCtx {
    pub device_id: DeviceId,
}

/// An authenticated device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthenticatedDevice {
    pub id: DeviceId,
    pub name: String,
}

/// Pairing failure.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PairError {
    #[error("the pairing code is invalid, used or expired")]
    InvalidCode,
    #[error("too many pairing attempts; try again later")]
    RateLimited,
    #[error("{0}")]
    Internal(String),
}

const ALLOWED_IMAGE_TYPES: [&str; 4] = ["image/png", "image/jpeg", "image/webp", "image/gif"];

/// A mutating request's idempotency key: the device and its `clientRequestId`.
type IdemKey = (DeviceId, String);

/// The longest `clientRequestId` the protocol allows (protocol.md §1.2: 1 to 128 characters).
const MAX_CLIENT_REQUEST_ID_CHARS: usize = 128;
/// Characters of the thread id's end in the default branch name of a worktree (`aas/<…>`,
/// protocol.md `thread/create`): unique among the threads of a project and short to type.
const WORKTREE_BRANCH_ID_CHARS: usize = 8;

pub struct Engine {
    sh: Arc<Shared>,
    actors: Mutex<HashMap<ThreadId, ActorHandle>>,
    /// Threads being removed: requests for them wait until the removal is decided (see
    /// [`Engine::actor`]).
    removing: Mutex<HashSet<ThreadId>>,
    /// Bumped when a removal starts and when it ends (under the `actors` lock): wakes the
    /// requests waiting for it, and tells an actor lookup that raced with one to look again.
    removals: watch::Sender<u64>,
    idem_locks: Mutex<HashMap<IdemKey, Arc<tokio::sync::Mutex<()>>>>,
    epoch: String,
    started: Instant,
    pairing_limiter: Mutex<RateLimiter>,
    revoked: broadcast::Sender<DeviceId>,
    /// Whether the actors have been stopped; held while [`Engine::shutdown`] stops them, so
    /// concurrent callers all return once everything is stopped.
    shutdown_done: tokio::sync::Mutex<bool>,
    stopped: AtomicBool,
}

fn params_hash(method: &str, params: &Value) -> String {
    let mut h = Sha256::new();
    h.update(method.as_bytes());
    h.update(b"\n");
    h.update(params.to_string().as_bytes());
    hex::encode(h.finalize())
}

impl Engine {
    /// Opens the database, recovers from a previous run, probes the harnesses and starts
    /// background maintenance.
    pub async fn start(
        config: EngineConfig,
        registry: HarnessRegistry,
        supervisor: Supervisor,
    ) -> CoreResult<Arc<Engine>> {
        config.policy.validate().map_err(CoreError::Internal)?;
        config.ensure_dirs()?;
        let db = crate::db::Db::open(&config.db_path(), config.policy.db_options())?;
        let epoch = db.write_blocking(|tx| {
            if let Some(e) = store::meta_get(tx, "epoch")? {
                return Ok(e);
            }
            let e = ulid::Ulid::generate().to_string();
            store::meta_set(tx, "epoch", &e)?;
            Ok(e)
        })?;
        let git = config.git.clone().map(|p| {
            Git::new(
                p,
                supervisor.clone(),
                config.tmp_dir(),
                config.policy.clone_timeout,
            )
        });
        let roots = canonical_roots(&config.project_roots);
        let (running_turns, _) = watch::channel(0usize);
        let (running_background, _) = watch::channel(0usize);
        let (storage_failure, _) = watch::channel(None);
        let sh = Arc::new(Shared {
            db,
            hub: HeadHub::new(),
            registry,
            supervisor,
            capacity: Capacity::new(config.policy.max_running_processes),
            blobs: BlobStore::new(config.blobs_dir(), config.tmp_dir()),
            git,
            fs: FsApi::new(roots, config.policy.file_index_ttl),
            running_turns,
            running_background,
            draining: AtomicBool::new(false),
            session_ending: AtomicBool::new(false),
            operations: OperationRegistry::default(),
            storage_failure,
            retention: tokio::sync::Mutex::new(()),
            harness_publish: tokio::sync::Mutex::new(()),
            config,
        });
        recover(&sh).await?;
        retention::sweep_leftovers(&sh).await?;
        // Probe in the background so the transport can start listening immediately; requests
        // that need harness information wait for `registry.wait_ready()`.
        let probe_sh = sh.clone();
        tokio::spawn(async move {
            let started = tokio::time::Instant::now();
            let probes: Vec<_> = probe_sh
                .registry
                .ids()
                .into_iter()
                .map(|id| probe_sh.probe_harness(id, Some(started), Publish::Always))
                .collect();
            for probe in futures::future::join_all(probes).await {
                if let Err(e) = probe {
                    tracing::error!(error = %e, "the initial probe of a harness failed");
                }
            }
            probe_sh.registry.mark_ready();
        });
        // Harnesses the probe found unavailable are probed again on the retry schedule.
        tokio::spawn(Shared::retry_unavailable_harnesses(Arc::downgrade(&sh)));
        let (revoked, _) = broadcast::channel(sh.config.policy.revocation_backlog);
        let engine = Arc::new(Engine {
            actors: Mutex::new(HashMap::new()),
            removing: Mutex::new(HashSet::new()),
            removals: watch::channel(0).0,
            idem_locks: Mutex::new(HashMap::new()),
            epoch,
            started: Instant::now(),
            pairing_limiter: Mutex::new(RateLimiter::new(
                sh.config.policy.pairing_attempts_per_window,
                sh.config.policy.pairing_rate_window,
            )),
            revoked,
            shutdown_done: tokio::sync::Mutex::new(false),
            stopped: AtomicBool::new(false),
            sh,
        });
        // Cleanup left over by a previous run (a removal the daemon stopped during).
        let cleanup_sh = engine.sh.clone();
        tokio::spawn(async move {
            let _running = cleanup_sh.retention.lock().await;
            if let Err(e) = retention::run_cleanup_jobs(&cleanup_sh, None).await {
                tracing::warn!(error = %e, "running the pending cleanup jobs failed; retried at the next maintenance");
            }
        });
        let weak = Arc::downgrade(&engine);
        tokio::spawn(async move {
            loop {
                let interval = match weak.upgrade() {
                    Some(e) => e.sh.config.policy.maintenance_interval,
                    None => return,
                };
                tokio::time::sleep(interval).await;
                let Some(engine) = weak.upgrade() else { return };
                if engine.stopped.load(Ordering::SeqCst) || engine.sh.failed() {
                    return;
                }
                if let Err(e) = engine.run_maintenance().await {
                    tracing::error!(error = %e, "maintenance failed");
                }
            }
        });
        // The fail-stop: once a write that must not be lost keeps failing, stop taking work
        // and stop every agent process (the daemon then tells clients and exits).
        let weak = Arc::downgrade(&engine);
        let mut failure = engine.sh.storage_failure.subscribe();
        tokio::spawn(async move {
            if failure.wait_for(Option::is_some).await.is_err() {
                return;
            }
            let Some(engine) = weak.upgrade() else { return };
            tracing::error!("fail-stop: stopping every agent process");
            engine.shutdown(false).await;
        });
        Ok(engine)
    }

    // ----- accessors for the transport ---------------------------------------------------------

    pub fn epoch(&self) -> &str {
        &self.epoch
    }

    pub fn policy(&self) -> &Policy {
        &self.sh.config.policy
    }

    pub fn config(&self) -> &EngineConfig {
        &self.sh.config
    }

    pub fn server_info(&self) -> ServerInfo {
        ServerInfo {
            name: self.sh.config.server_name.clone(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            hostname: self.sh.config.hostname.clone(),
            epoch: self.epoch.clone(),
        }
    }

    pub fn uptime_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    pub fn running_turns(&self) -> usize {
        *self.sh.running_turns.borrow()
    }

    pub fn running_processes(&self) -> usize {
        self.sh.capacity.in_use()
    }

    /// Background tasks that keep an agent busy (in their harness's live set and not ambient),
    /// over all threads.
    pub fn running_background_tasks(&self) -> usize {
        *self.sh.running_background.borrow()
    }

    pub fn is_draining(&self) -> bool {
        self.sh.is_draining()
    }

    /// The storage failure the daemon fail-stopped for, if it did (design.md §6.2).
    pub fn storage_failure(&self) -> Option<StorageFailure> {
        self.sh.storage_failure.borrow().clone()
    }

    /// Resolves once the daemon has fail-stopped (with the failure).
    pub async fn wait_storage_failure(&self) -> StorageFailure {
        let mut rx = self.sh.storage_failure.subscribe();
        loop {
            if let Some(failure) = rx.borrow_and_update().clone() {
                return failure;
            }
            if rx.changed().await.is_err() {
                // The sender lives as long as the engine's shared state, which outlives `self`.
                std::future::pending::<()>().await;
            }
        }
    }

    /// Why the server is shutting down, for `server/shuttingDown`: a storage failure, a
    /// drain, or a plain stop.
    pub fn shutdown_reason(&self) -> ShutdownReason {
        if self.sh.failed() {
            ShutdownReason::StorageFailure
        } else if self.sh.is_draining() {
            ShutdownReason::Drain
        } else {
            ShutdownReason::Shutdown
        }
    }

    /// Devices revoked while connected (the transport closes their connections).
    pub fn revocations(&self) -> broadcast::Receiver<DeviceId> {
        self.revoked.subscribe()
    }

    /// Head of a stream if it exists (workspace, or a non-removed thread).
    pub async fn stream_head(&self, stream: &str) -> CoreResult<Option<u64>> {
        let Some(sref) = parse_stream(stream) else {
            return Ok(None);
        };
        let stream = stream.to_owned();
        self.sh
            .db
            .read(move |tx| {
                if let StreamRef::Thread(id) = &sref {
                    match store::get_thread(tx, id)? {
                        Some(row) if !row.removed => {}
                        _ => return Ok(None),
                    }
                }
                Ok(Some(aas_eventlog::head(tx, &stream)?))
            })
            .await
    }

    /// Reads the next batch of `stream` after `after`.
    pub async fn read_batch(&self, stream: String, after: u64) -> CoreResult<Batch> {
        let limits = self.sh.config.policy.batch_limits();
        self.sh
            .db
            .read(move |tx| Ok(aas_eventlog::read_batch(tx, &stream, after, limits)?))
            .await
    }

    /// Head notifications of `stream`. Subscribe *before* reading the log, then wait for a
    /// change once the read found nothing new (see [`HeadHub::subscribe`]).
    pub fn subscribe_head(&self, stream: &str) -> watch::Receiver<u64> {
        self.sh.hub.subscribe(stream)
    }

    /// Current heads of `streams` as stored in the log (0 for streams without events).
    pub async fn stream_heads(
        &self,
        streams: Vec<String>,
    ) -> CoreResult<std::collections::BTreeMap<String, u64>> {
        self.sh
            .db
            .read(move |tx| Ok(aas_eventlog::heads(tx, &streams)?.into_iter().collect()))
            .await
    }

    // ----- devices, pairing ------------------------------------------------------------------

    pub async fn authenticate(&self, token: &str) -> CoreResult<Option<AuthenticatedDevice>> {
        let hash = auth::hash_secret(token);
        let device = self
            .sh
            .db
            .read(move |tx| store::device_by_token_hash(tx, &hash))
            .await?;
        Ok(device.filter(|d| !d.revoked).map(|d| AuthenticatedDevice {
            id: d.id,
            name: d.name,
        }))
    }

    /// Whether the device is paired and not revoked.
    pub async fn device_active(&self, id: &DeviceId) -> CoreResult<bool> {
        let id = id.clone();
        let devices = self.sh.db.read(|tx| store::list_devices(tx)).await?;
        Ok(devices.iter().any(|d| d.id == id))
    }

    pub async fn touch_device(&self, id: &DeviceId) -> CoreResult<()> {
        let id = id.clone();
        self.sh
            .db
            .write(move |tx| store::touch_device(tx, &id, now_ms()))
            .await
    }

    /// A new single-use pairing code and its expiry.
    pub async fn create_pairing_code(&self) -> CoreResult<(String, Millis)> {
        let code = auth::new_pairing_code();
        let hash = auth::hash_code(&code);
        let now = now_ms();
        let expires = now + self.sh.config.policy.pairing_code_ttl.as_millis() as i64;
        self.sh
            .db
            .write(move |tx| store::insert_pairing_code(tx, &hash, now, expires))
            .await?;
        Ok((code, expires))
    }

    pub async fn pair(
        &self,
        code: &str,
        device_name: &str,
        platform: &str,
    ) -> Result<PairResponse, PairError> {
        if !self.pairing_limiter.lock().allow() {
            return Err(PairError::RateLimited);
        }
        let hash = auth::hash_code(code);
        let token = auth::new_token();
        let token_hash = auth::hash_secret(&token);
        let policy = &self.sh.config.policy;
        let device = store::DeviceRow {
            id: DeviceId::generate(),
            name: device_name
                .trim()
                .chars()
                .take(policy.device_name_chars)
                .collect::<String>(),
            platform: Some(
                platform
                    .chars()
                    .take(policy.device_platform_chars)
                    .collect(),
            ),
            created_at: now_ms(),
            last_seen_at: None,
            revoked: false,
        };
        let row = device.clone();
        let ok = self
            .sh
            .db
            .write(move |tx| {
                if !store::consume_pairing_code(tx, &hash, now_ms())? {
                    return Ok(false);
                }
                store::insert_device(tx, &row, &token_hash)?;
                Ok(true)
            })
            .await
            .map_err(|e| PairError::Internal(e.to_string()))?;
        if !ok {
            return Err(PairError::InvalidCode);
        }
        tracing::info!(device = %device.id, name = %device.name, "device paired");
        Ok(PairResponse {
            device_id: device.id,
            token,
            server: PairServerInfo {
                name: self.sh.config.server_name.clone(),
                epoch: self.epoch.clone(),
            },
        })
    }

    pub async fn list_devices(&self, current: Option<&DeviceId>) -> CoreResult<Vec<Device>> {
        let current = current.cloned();
        let rows = self.sh.db.read(|tx| store::list_devices(tx)).await?;
        Ok(rows
            .into_iter()
            .map(|d| Device {
                current: current.as_ref() == Some(&d.id),
                id: d.id,
                name: d.name,
                platform: d.platform,
                created_at: d.created_at,
                last_seen_at: d.last_seen_at,
            })
            .collect())
    }

    pub async fn revoke_device(&self, id: &DeviceId) -> CoreResult<bool> {
        let target = id.clone();
        let revoked = self
            .sh
            .db
            .write(move |tx| store::revoke_device(tx, &target, now_ms()))
            .await?;
        if revoked {
            let _ = self.revoked.send(id.clone());
        }
        Ok(revoked)
    }

    // ----- blobs -------------------------------------------------------------------------------

    pub async fn put_blob(&self, bytes: Vec<u8>, mime: &str) -> CoreResult<BlobUploadResponse> {
        self.accepting_requests().map_err(CoreError::Rpc)?;
        let mime = mime
            .split(';')
            .next()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        if !ALLOWED_IMAGE_TYPES.contains(&mime.as_str()) {
            return Err(invalid_params(format!("unsupported content type {mime}")));
        }
        if bytes.len() as u64 > self.sh.config.policy.max_blob_bytes {
            return Err(rpc(
                ErrorKind::PayloadTooLarge,
                "the upload exceeds policy.max_blob_bytes",
            ));
        }
        let store = self.sh.blobs.clone();
        let (id, size, pin) = tokio::task::spawn_blocking(move || store.put_bytes(&bytes))
            .await
            .map_err(|e| CoreError::Internal(e.to_string()))??;
        // Unreferenced until a message refers to it: kept for `unreferenced_blob_grace`.
        let (row_id, row_mime) = (id.clone(), mime.clone());
        self.sh
            .db
            .write(move |tx| store::insert_blob(tx, &row_id, &row_mime, size, now_ms()))
            .await?;
        drop(pin);
        Ok(BlobUploadResponse {
            blob_id: id,
            mime,
            size,
        })
    }

    /// Path and content type of a stored blob.
    pub async fn blob(&self, id: &BlobId) -> CoreResult<Option<(PathBuf, String)>> {
        let Some(path) = self.sh.blobs.path_of(id) else {
            return Ok(None);
        };
        let key = id.clone();
        let meta = self.sh.db.read(move |tx| store::get_blob(tx, &key)).await?;
        Ok(meta.filter(|_| path.exists()).map(|(mime, _)| (path, mime)))
    }

    // ----- requests ----------------------------------------------------------------------------

    /// After the fail-stop every request is refused with the non-definitive `draining`: the
    /// client keeps it and sends it again to the restarted daemon.
    fn accepting_requests(&self) -> Result<(), RpcError> {
        match self.sh.failed() {
            true => Err(RpcError::new(
                ErrorKind::Draining,
                "the server is stopping after a storage failure; it restarts shortly",
            )),
            false => Ok(()),
        }
    }

    /// Handles one client request (except `initialize`/`subscribe`/`unsubscribe`, which the
    /// transport owns). Mutating requests are idempotent per `(device, clientRequestId)`.
    pub async fn handle(
        self: &Arc<Self>,
        ctx: &RequestCtx,
        req: ClientRequest,
    ) -> Result<Value, RpcError> {
        self.accepting_requests()?;
        let Some(crid) = req.client_request_id().map(str::to_owned) else {
            return self.dispatch(ctx, req, None).await.map_err(RpcError::from);
        };
        if crid.is_empty() || crid.chars().count() > MAX_CLIENT_REQUEST_ID_CHARS {
            return Err(RpcError::invalid_params(format!(
                "clientRequestId must be 1..={MAX_CLIENT_REQUEST_ID_CHARS} characters"
            )));
        }
        let idem = Idem {
            device: ctx.device_id.clone(),
            client_request_id: crid.clone(),
            method: req.method(),
            params_hash: params_hash(req.method(), &req.params_json()),
        };
        let key = (ctx.device_id.clone(), crid.clone());
        let lock = self
            .idem_locks
            .lock()
            .entry(key.clone())
            .or_default()
            .clone();
        let guard = lock.lock().await;
        let result = self.handle_idempotent(ctx, req, idem).await;
        drop(guard);
        let mut locks = self.idem_locks.lock();
        if locks.get(&key).is_some_and(|l| Arc::strong_count(l) <= 2) {
            locks.remove(&key);
        }
        result
    }

    async fn handle_idempotent(
        self: &Arc<Self>,
        ctx: &RequestCtx,
        req: ClientRequest,
        idem: Idem,
    ) -> Result<Value, RpcError> {
        let (dev, crid) = (idem.device.clone(), idem.client_request_id.clone());
        let existing = self
            .sh
            .db
            .read(move |tx| store::idem_get(tx, &dev, &crid))
            .await?;
        if let Some(rec) = existing {
            if rec.method != idem.method || rec.params_hash != idem.params_hash {
                return Err(RpcError::new(
                    ErrorKind::IdempotencyKeyReused,
                    "clientRequestId was already used for a different request",
                ));
            }
            tracing::debug!(method = idem.method, crid = %idem.client_request_id, "replaying stored result");
            return match (rec.response.get("result"), rec.response.get("error")) {
                (Some(r), _) => Ok(r.clone()),
                (None, Some(e)) => Err(serde_json::from_value(e.clone())
                    .unwrap_or_else(|_| RpcError::internal("corrupt stored error"))),
                _ => Err(RpcError::internal("corrupt idempotency record")),
            };
        }
        let result = self
            .dispatch(ctx, req, Some(idem.clone()))
            .await
            .map_err(RpcError::from);
        match &result {
            Ok(value) => {
                // Handlers store the result atomically with their state change; this is the
                // safety net for handlers without state changes.
                let (dev, crid) = (idem.device.clone(), idem.client_request_id.clone());
                let stored = self
                    .sh
                    .db
                    .read(move |tx| store::idem_get(tx, &dev, &crid))
                    .await?;
                if stored.is_none() {
                    let (i, v) = (idem.clone(), value.clone());
                    self.sh.db.write(move |tx| i.store_result(tx, &v)).await?;
                }
            }
            Err(e) if e.kind().is_some_and(ErrorKind::is_definitive) => {
                let (i, err) = (idem.clone(), e.clone());
                let stored = self
                    .sh
                    .db
                    .write(move |tx| {
                        if store::idem_get(tx, &i.device, &i.client_request_id)?.is_none() {
                            i.store_error(tx, &err)?;
                        }
                        Ok(())
                    })
                    .await;
                if let Err(store_error) = stored {
                    // A resend runs the request again, which gives the same definitive error.
                    tracing::warn!(method = idem.method, error = %store_error, "could not store a definitive error for resends");
                }
            }
            Err(_) => {}
        }
        result
    }

    async fn dispatch(
        self: &Arc<Self>,
        ctx: &RequestCtx,
        req: ClientRequest,
        idem: Option<Idem>,
    ) -> CoreResult<Value> {
        use ClientRequest as R;
        Ok(match req {
            R::Initialize(_) | R::Subscribe(_) | R::Unsubscribe(_) => {
                return Err(rpc(ErrorKind::InvalidRequest, "handled by the transport"));
            }
            R::WorkspaceSnapshot(_) => to_value(self.workspace_snapshot().await?)?,
            R::ServerStatus(_) => to_value(ServerStatusResult {
                uptime_ms: self.uptime_ms(),
                running_processes: self.running_processes() as u32,
                running_turns: self.running_turns() as u32,
                draining: self.is_draining(),
                prevent_sleep_while_running: self.sh.config.policy.prevent_sleep_while_running,
                running_background_tasks: self.running_background_tasks() as u32,
            })?,
            R::DeviceList(_) => to_value(DeviceListResult {
                devices: self.list_devices(Some(&ctx.device_id)).await?,
            })?,
            R::DeviceRevoke(p) => {
                if !self.revoke_device(&p.device_id).await? {
                    return Err(not_found("device", &p.device_id));
                }
                json!({})
            }
            R::HarnessList(_) => {
                self.sh.registry.wait_ready().await;
                to_value(HarnessListResult {
                    harnesses: self.sh.registry.harnesses(),
                })?
            }
            R::HarnessRefresh(p) => to_value(HarnessListResult {
                harnesses: self.refresh_harnesses(p.harness_id.as_deref()).await?,
            })?,
            R::ProjectList(p) => {
                let projects = self
                    .sh
                    .db
                    .read(move |tx| store::list_projects(tx, p.include_archived))
                    .await?;
                to_value(ProjectListResult {
                    projects: projects.into_iter().map(with_git).collect(),
                })?
            }
            R::ProjectGet(p) => to_value(ProjectResult {
                project: self.project(&p.project_id).await?,
            })?,
            R::ProjectCreate(p) => to_value(self.project_create(p, idem).await?)?,
            R::ProjectOpen(p) => to_value(self.project_open(p, idem).await?)?,
            R::ProjectUpdate(p) => to_value(self.project_update(p, idem).await?)?,
            R::ProjectArchive(p) => to_value(self.project_archive(p, idem).await?)?,
            R::ProjectRemove(p) => {
                self.project_remove(p, idem).await?;
                json!({})
            }
            R::FsRoots(_) => to_value(FsRootsResult {
                roots: self.sh.fs.roots(),
            })?,
            R::FsList(p) => {
                let (path, entries) = self.sh.fs.list(Path::new(&p.path), p.include_files)?;
                to_value(FsListResult {
                    path: path.display().to_string(),
                    entries,
                })?
            }
            R::FsMkdir(p) => {
                let path = self.sh.fs.mkdir(Path::new(&p.path))?;
                to_value(FsMkdirResult {
                    path: path.display().to_string(),
                })?
            }
            R::FsSearch(p) => to_value(self.fs_search(p).await?)?,
            R::ThreadList(p) => to_value(self.thread_list(p).await?)?,
            R::ThreadGet(p) => to_value(ThreadResult {
                thread: self.thread_view(&p.thread_id).await?,
            })?,
            R::ThreadCreate(p) => to_value(self.thread_create(p, idem).await?)?,
            R::ThreadRead(p) => to_value(self.thread_read(p).await?)?,
            R::ThreadUpdate(p) => {
                let id = p.thread_id.clone();
                to_value(
                    self.ask(&id, |reply| Msg::Update {
                        title: p.title,
                        settings: p.settings,
                        pinned: p.pinned,
                        idem,
                        reply,
                    })
                    .await?,
                )?
            }
            R::ThreadArchive(p) => {
                let id = p.thread_id.clone();
                to_value(
                    self.ask(&id, |reply| Msg::Archive {
                        archived: p.archived,
                        remove_worktree: p.remove_worktree,
                        force: p.force,
                        idem,
                        reply,
                    })
                    .await?,
                )?
            }
            R::ThreadFork(p) => to_value(self.thread_fork(p, idem).await?)?,
            R::ThreadStop(p) => {
                let id = p.thread_id.clone();
                to_value(self.ask(&id, |reply| Msg::Stop { idem, reply }).await?)?
            }
            R::ThreadDiff(p) => to_value(self.thread_diff(p).await?)?,
            R::TurnStart(p) => {
                let id = p.thread_id.clone();
                to_value(
                    self.ask_with_harness(&id, |reply| Msg::StartTurn {
                        input: p.input,
                        delivery: p.delivery,
                        idem,
                        create: false,
                        reply,
                    })
                    .await?,
                )?
            }
            R::TurnInterrupt(p) => {
                let id = p.thread_id.clone();
                to_value(
                    self.ask(&id, |reply| Msg::Interrupt { idem, reply })
                        .await?,
                )?
            }
            R::QueueRemove(p) => {
                let id = p.thread_id.clone();
                to_value(
                    self.ask(&id, |reply| Msg::RemoveQueued {
                        queued_id: p.queued_id,
                        idem,
                        reply,
                    })
                    .await?,
                )?
            }
            R::QueueResume(p) => {
                let id = p.thread_id.clone();
                to_value(
                    self.ask_with_harness(&id, |reply| Msg::ResumeQueue { idem, reply })
                        .await?,
                )?
            }
            R::QueueUpdate(p) => {
                let id = p.thread_id.clone();
                to_value(
                    self.ask_with_harness(&id, |reply| Msg::UpdateQueued {
                        queued_id: p.queued_id,
                        input: p.input,
                        idem,
                        reply,
                    })
                    .await?,
                )?
            }
            R::QueueSteer(p) => {
                let id = p.thread_id.clone();
                to_value(
                    self.ask_with_harness(&id, |reply| Msg::SteerQueued {
                        queued_id: p.queued_id,
                        idem,
                        reply,
                    })
                    .await?,
                )?
            }
            R::InteractionRespond(p) => {
                let iid = p.interaction_id.clone();
                let row = self
                    .sh
                    .db
                    .read(move |tx| store::get_interaction(tx, &iid))
                    .await?;
                let row = row.ok_or_else(|| not_found("interaction", &p.interaction_id))?;
                let device = ctx.device_id.clone();
                to_value(
                    self.ask(&row.interaction.thread_id, |reply| Msg::Respond {
                        interaction_id: p.interaction_id,
                        resolution: p.resolution,
                        device,
                        idem,
                        reply,
                    })
                    .await?,
                )?
            }
            R::InteractionList(p) => {
                let status = p.status;
                let rows = self
                    .sh
                    .db
                    .read(|tx| store::pending_interactions(tx))
                    .await?;
                let interactions = rows
                    .into_iter()
                    .map(|r| r.interaction)
                    .filter(|i| status.is_none_or(|s| s == i.status))
                    .collect();
                to_value(InteractionListResult { interactions })?
            }
            R::CommandList(p) => to_value(self.command_list(p).await?)?,
            R::NativeList(p) => to_value(self.native_list(p).await?)?,
            R::NativeImport(p) => to_value(self.native_import(p, idem).await?)?,
            R::OperationList(_) => {
                let limit = self.sh.config.policy.operation_list_limit;
                let operations = self
                    .sh
                    .db
                    .read(move |tx| store::list_operations(tx, limit))
                    .await?;
                to_value(OperationListResult { operations })?
            }
            R::OperationCancel(p) => to_value(self.operation_cancel(p).await?)?,
            R::BackgroundTaskStop(p) => {
                let id = p.thread_id.clone();
                to_value(
                    self.ask(&id, |reply| Msg::StopBackground {
                        task_id: p.task_id,
                        idem,
                        reply,
                    })
                    .await?,
                )?
            }
        })
    }

    // ----- actors ------------------------------------------------------------------------------

    /// The actor of `thread_id` (spawned when it has none).
    ///
    /// A request for a thread that is being removed waits until the removal is decided: once
    /// the thread is gone it gets `notFound` (definitive: stored for resends, and true), and
    /// when the removal failed it is handled as if nothing had happened. Answering `notFound`
    /// right away would be wrong in the second case, and a stored definitive error is never
    /// taken back.
    async fn actor(&self, thread_id: &ThreadId) -> CoreResult<ActorHandle> {
        loop {
            let mut removals = self.removals.subscribe();
            let generation = *removals.borrow_and_update();
            match self.lookup_actor(thread_id, generation).await? {
                Lookup::Found(handle) => return Ok(handle),
                Lookup::Removing => {
                    // The sender lives as long as the engine.
                    let _ = removals.changed().await;
                }
                Lookup::Retry => {}
            }
        }
    }

    async fn lookup_actor(&self, thread_id: &ThreadId, generation: u64) -> CoreResult<Lookup> {
        {
            let actors = self.actors.lock();
            if self.removing.lock().contains(thread_id) {
                return Ok(Lookup::Removing);
            }
            if let Some(h) = actors.get(thread_id).filter(|h| !h.is_closed()) {
                return Ok(Lookup::Found(h.clone()));
            }
        }
        let id = thread_id.clone();
        let preview_chars = self.sh.config.policy.queued_preview_chars;
        let (row, queue_len, next_index, last_turn) = self
            .sh
            .db
            .read(move |tx| {
                let row = store::get_thread(tx, &id)?.ok_or_else(|| not_found("thread", &id))?;
                let queue = store::list_queued(tx, &id, preview_chars)?.len();
                let next = store::next_turn_index(tx, &id)?;
                let last = store::last_turn(tx, &id)?.map(|t| t.turn.id);
                Ok((row, queue, next, last))
            })
            .await?;
        let mut actors = self.actors.lock();
        if *self.removals.borrow() != generation {
            // A removal started or ended while the row was read: it may be stale.
            return Ok(Lookup::Retry);
        }
        if self.removing.lock().contains(thread_id) {
            return Ok(Lookup::Removing);
        }
        if row.removed {
            return Err(not_found("thread", thread_id));
        }
        let handle = match actors.get(thread_id).filter(|h| !h.is_closed()) {
            Some(h) => h.clone(),
            None => {
                let h = actor::spawn(self.sh.clone(), row, true, queue_len, next_index, last_turn);
                actors.insert(thread_id.clone(), h.clone());
                h
            }
        };
        Ok(Lookup::Found(handle))
    }

    async fn ask<T>(
        &self,
        thread_id: &ThreadId,
        make: impl FnOnce(Reply<T>) -> Msg,
    ) -> CoreResult<T> {
        let handle = self.actor(thread_id).await?;
        Self::send_to(&handle, make).await
    }

    /// [`ask`](Self::ask) for a request that starts or checks a turn and so needs the thread's
    /// harness: an unavailable harness is probed again first (design.md §9.4), outside the
    /// actor, so that the actor keeps handling the thread's other requests meanwhile.
    async fn ask_with_harness<T>(
        &self,
        thread_id: &ThreadId,
        make: impl FnOnce(Reply<T>) -> Msg,
    ) -> CoreResult<T> {
        let handle = self.actor(thread_id).await?;
        self.sh
            .recheck_unavailable_harness(handle.harness_id())
            .await;
        Self::send_to(&handle, make).await
    }

    async fn send_to<T>(handle: &ActorHandle, make: impl FnOnce(Reply<T>) -> Msg) -> CoreResult<T> {
        let (tx, rx) = oneshot::channel();
        handle
            .send(make(tx))
            .map_err(|_| CoreError::Internal("thread actor is gone".into()))?;
        rx.await
            .map_err(|_| CoreError::Internal("thread actor dropped the request".into()))?
            .map_err(CoreError::Rpc)
    }

    // ----- workspace & projects ---------------------------------------------------------------

    async fn workspace_snapshot(&self) -> CoreResult<WorkspaceSnapshotResult> {
        self.sh.registry.wait_ready().await;
        let harnesses = self.sh.registry.harnesses();
        let operation_limit = self.sh.config.policy.snapshot_operation_limit;
        self.sh
            .db
            .read(move |tx| {
                let projects = store::list_projects(tx, false)?
                    .into_iter()
                    .map(with_git)
                    .collect();
                let rows = store::all_threads(tx, false)?;
                let threads = rows
                    .iter()
                    .map(|r| store::thread_view(tx, r))
                    .collect::<CoreResult<Vec<_>>>()?;
                let pending_interactions = store::pending_interactions(tx)?
                    .into_iter()
                    .map(|r| r.interaction)
                    .collect();
                let operations = store::list_operations(tx, operation_limit)?;
                let head = aas_eventlog::head(tx, WORKSPACE_STREAM)?;
                Ok(WorkspaceSnapshotResult {
                    harnesses,
                    projects,
                    threads,
                    pending_interactions,
                    operations,
                    head,
                })
            })
            .await
    }

    async fn project(&self, id: &ProjectId) -> CoreResult<Project> {
        let key = id.clone();
        let row = self
            .sh
            .db
            .read(move |tx| store::get_project(tx, &key))
            .await?;
        match row {
            Some(r) if !r.removed => Ok(with_git(r.project)),
            _ => Err(not_found("project", id)),
        }
    }

    async fn register_project(
        &self,
        path: PathBuf,
        name: String,
        idem: Option<Idem>,
    ) -> CoreResult<Project> {
        let project = self
            .sh
            .tx(move |tx, em| {
                let project = upsert_project(tx, em, &path, name)?;
                if let Some(idem) = &idem {
                    match idem.method {
                        "project/create" => idem.store_result(
                            tx,
                            &ProjectCreateResult {
                                project: Some(project.clone()),
                                operation: None,
                            },
                        )?,
                        _ => idem.store_result(
                            tx,
                            &ProjectResult {
                                project: project.clone(),
                            },
                        )?,
                    }
                }
                Ok(project)
            })
            .await?;
        Ok(project)
    }

    async fn project_create(
        &self,
        p: ProjectCreateParams,
        idem: Option<Idem>,
    ) -> CoreResult<ProjectCreateResult> {
        let target = self.sh.fs.allowed_new(Path::new(&p.parent_path), &p.name)?;
        match p.init {
            ProjectInit::Empty | ProjectInit::GitInit => {
                let dir = self.sh.fs.mkdir(&target)?;
                if matches!(p.init, ProjectInit::GitInit) && !dir.join(".git").exists() {
                    let git = self
                        .sh
                        .git
                        .clone()
                        .ok_or_else(|| invalid_state("git is not available on this PC"))?;
                    git.init(&dir).await?;
                }
                let project = self.register_project(dir, p.name, idem).await?;
                Ok(ProjectCreateResult {
                    project: Some(project),
                    operation: None,
                })
            }
            ProjectInit::GitClone { url } => {
                let git = self
                    .sh
                    .git
                    .clone()
                    .ok_or_else(|| invalid_state("git is not available on this PC"))?;
                if target.exists() {
                    return Err(rpc(
                        ErrorKind::AlreadyExists,
                        format!("{} already exists", target.display()),
                    ));
                }
                if url.trim().is_empty() || url.starts_with('-') {
                    return Err(invalid_params("invalid repository URL"));
                }
                let op = Operation {
                    id: OperationId::generate(),
                    kind: OperationKind::GitClone,
                    status: OperationStatus::Running,
                    project_id: None,
                    message: Some(format!("Cloning {url}")),
                    progress: None,
                    started_at: now_ms(),
                    finished_at: None,
                };
                let work_dir = operations::clone_work_dir(&target, &op.id).ok_or_else(|| {
                    invalid_params(format!(
                        "{} is not a valid clone destination",
                        target.display()
                    ))
                })?;
                let result = ProjectCreateResult {
                    project: None,
                    operation: Some(op.clone()),
                };
                let job = CloneJob {
                    op,
                    url,
                    target,
                    work_dir,
                    name: p.name,
                    git,
                };
                operations::start_clone(&self.sh, job, idem).await?;
                Ok(result)
            }
        }
    }

    /// Cancels a running operation and answers with its final state (an operation that has
    /// already ended is returned as it is).
    async fn operation_cancel(&self, p: OperationCancelParams) -> CoreResult<OperationResult> {
        self.sh.operations.cancel(&p.operation_id).await;
        let id = p.operation_id.clone();
        let op = self
            .sh
            .db
            .read(move |tx| store::get_operation(tx, &id))
            .await?;
        let operation = op.ok_or_else(|| not_found("operation", &p.operation_id))?;
        if operation.status == OperationStatus::Running {
            // Only operations of this process run (the rest were ended at startup), so a
            // running record here means its task has not recorded the end yet: impossible
            // after `cancel` returned.
            return Err(CoreError::Internal(format!(
                "operation {} is still running after its cancellation",
                operation.id
            )));
        }
        Ok(OperationResult { operation })
    }

    async fn project_open(
        &self,
        p: ProjectOpenParams,
        idem: Option<Idem>,
    ) -> CoreResult<ProjectResult> {
        let dir = self.sh.fs.allowed_existing(Path::new(&p.path))?;
        if !dir.is_dir() {
            return Err(invalid_params(format!(
                "{} is not a directory",
                dir.display()
            )));
        }
        let name = match p.name {
            Some(n) if !n.trim().is_empty() => n.trim().to_owned(),
            _ => dir
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| dir.display().to_string()),
        };
        Ok(ProjectResult {
            project: self.register_project(dir, name, idem).await?,
        })
    }

    async fn project_update(
        &self,
        p: ProjectUpdateParams,
        idem: Option<Idem>,
    ) -> CoreResult<ProjectResult> {
        if let Some(defaults) = &p.defaults
            && let Some(h) = &defaults.harness_id
            && self.sh.registry.get(h).is_none()
        {
            return Err(invalid_params(format!("unknown harness {h}")));
        }
        let project = self
            .sh
            .tx_idem(idem, move |tx, em| {
                let mut row = store::get_project(tx, &p.project_id)?
                    .filter(|r| !r.removed)
                    .ok_or_else(|| not_found("project", &p.project_id))?;
                if let Some(name) = p.name {
                    let name = name.trim().to_owned();
                    if name.is_empty() {
                        return Err(invalid_params("name must not be empty"));
                    }
                    row.project.name = name;
                }
                if let Some(d) = p.defaults {
                    row.project.defaults = d;
                }
                row.project.updated_at = now_ms();
                store::update_project(tx, &row.project, false)?;
                let project = with_git(row.project);
                em.workspace(Event::ProjectUpserted {
                    project: project.clone(),
                });
                Ok(ProjectResult { project })
            })
            .await?;
        Ok(project)
    }

    async fn project_archive(
        &self,
        p: ProjectArchiveParams,
        idem: Option<Idem>,
    ) -> CoreResult<ProjectResult> {
        self.sh
            .tx_idem(idem, move |tx, em| {
                let mut row = store::get_project(tx, &p.project_id)?
                    .filter(|r| !r.removed)
                    .ok_or_else(|| not_found("project", &p.project_id))?;
                row.project.archived = p.archived;
                row.project.updated_at = now_ms();
                store::update_project(tx, &row.project, false)?;
                let project = with_git(row.project);
                em.workspace(Event::ProjectUpserted {
                    project: project.clone(),
                });
                Ok(ProjectResult { project })
            })
            .await
    }

    /// Removes a project and purges everything stored about its threads (design.md §6.1).
    /// The project's own row stays (marked removed), so opening the folder again brings back
    /// its id and defaults. Snapshot refs and the worktrees the daemon created are cleanup
    /// jobs, run right away and retried by maintenance until they succeed.
    async fn project_remove(&self, p: ProjectRemoveParams, idem: Option<Idem>) -> CoreResult<()> {
        let pid = p.project_id.clone();
        let (project, threads) = self
            .sh
            .db
            .read(move |tx| {
                let project = store::get_project(tx, &pid)?
                    .filter(|r| !r.removed)
                    .ok_or_else(|| not_found("project", &pid))?;
                Ok((project.project, store::threads_of_project(tx, &pid)?))
            })
            .await?;
        if threads.iter().any(|t| t.status != ThreadStatus::Idle) {
            return Err(invalid_state("stop the project's running threads first"));
        }
        // From here on requests for these threads wait for the outcome; the actors that exist
        // confirm they are idle and hold what they receive meanwhile.
        let ids: Vec<ThreadId> = threads.iter().map(|t| t.id.clone()).collect();
        {
            let _actors = self.actors.lock();
            self.removing.lock().extend(ids.iter().cloned());
            self.removals.send_modify(|g| *g += 1);
        }
        let mut retired = Vec::new();
        let result = self
            .remove_threads_and_project(&project, &threads, idem, &mut retired)
            .await;
        {
            let mut actors = self.actors.lock();
            let mut removing = self.removing.lock();
            match &result {
                // Retired actors exit once their handles are gone, answering what they held
                // with `notFound`.
                Ok(_) => {
                    for id in &ids {
                        actors.remove(id);
                    }
                }
                // Nothing was removed: the retired actors carry on (with what they held), and
                // the actors that were never retired were never touched.
                Err(_) => {
                    for handle in &retired {
                        let _ = handle.send(Msg::Unretire);
                    }
                }
            }
            for id in &ids {
                removing.remove(id);
            }
            self.removals.send_modify(|g| *g += 1);
        }
        let jobs = result?;
        if !jobs.is_empty() {
            let _running = self.sh.retention.lock().await;
            let (done, pending) = retention::run_cleanup_jobs(&self.sh, Some(&jobs)).await?;
            tracing::info!(project = %project.id, done, pending, "removed project cleaned up");
        }
        Ok(())
    }

    /// The part of [`project_remove`](Self::project_remove) that runs while the threads are
    /// marked as being removed. Returns the cleanup jobs it created.
    /// Retired actors are added to `retired` (the caller releases them when this fails).
    async fn remove_threads_and_project(
        &self,
        project: &Project,
        threads: &[ThreadRow],
        idem: Option<Idem>,
        retired: &mut Vec<ActorHandle>,
    ) -> CoreResult<Vec<i64>> {
        for t in threads {
            let handle = self.actors.lock().get(&t.id).cloned();
            if let Some(h) = handle {
                let (tx, rx) = oneshot::channel();
                if h.send(Msg::Retire { reply: tx }).is_ok() {
                    rx.await
                        .map_err(|_| CoreError::Internal("thread actor is gone".into()))??;
                    retired.push(h);
                }
            }
        }
        // Worktrees are removed without force: refuse before anything is removed when one
        // has uncommitted changes (thread/archive with removeWorktree and force discards them).
        // Without a reachable repository git cannot tell; the removal is then deferred to the
        // cleanup job, which waits until the repository can be reached and never removes a
        // worktree git has not checked (`retention::run_cleanup_job`).
        let worktrees: BTreeSet<String> = threads
            .iter()
            .filter_map(|t| match &t.workspace {
                Workspace::Worktree { path, .. } if Path::new(path).exists() => Some(path.clone()),
                _ => None,
            })
            .collect();
        if let Some(git) = &self.sh.git
            && quick_info(Path::new(&project.path)).is_repo
        {
            for path in &worktrees {
                let clean = git.worktree_is_clean(Path::new(path)).await.map_err(|e| {
                    invalid_state(format!(
                        "the worktree {path} cannot be checked, so it is not removed: {e}"
                    ))
                })?;
                if !clean {
                    return Err(invalid_state(format!(
                        "the worktree {path} has uncommitted changes; commit them, or archive its thread with removeWorktree and force to discard them"
                    )));
                }
            }
        }
        let repo = project.path.clone();
        let project_id = project.id.clone();
        let thread_ids: Vec<ThreadId> = threads.iter().map(|t| t.id.clone()).collect();
        let worktree_dirs: Vec<String> = worktrees.into_iter().collect();
        self.sh
            .tx(move |tx, em| {
                let now = now_ms();
                let mut row = store::get_project(tx, &project_id)?
                    .filter(|r| !r.removed)
                    .ok_or_else(|| not_found("project", &project_id))?;
                let mut jobs = Vec::new();
                for id in &thread_ids {
                    em.workspace(Event::ThreadRemoved {
                        thread_id: id.clone(),
                    });
                    // Written before the purge, which keeps `thread/removed` for the clients
                    // that were offline.
                    em.flush(tx)?;
                    let purged = store::purge_thread(tx, id, now)?;
                    tracing::debug!(thread = %id, ?purged, "purged a removed thread");
                    jobs.push(store::insert_cleanup_job(
                        tx,
                        CleanupKind::SnapshotRefs,
                        &repo,
                        id.as_str(),
                        now,
                    )?);
                }
                for path in &worktree_dirs {
                    jobs.push(store::insert_cleanup_job(
                        tx,
                        CleanupKind::Worktree,
                        &repo,
                        path,
                        now,
                    )?);
                }
                row.project.updated_at = now;
                store::update_project(tx, &row.project, true)?;
                em.workspace(Event::ProjectRemoved {
                    project_id: project_id.clone(),
                });
                if let Some(idem) = &idem {
                    idem.store_result(tx, &json!({}))?;
                }
                Ok(jobs)
            })
            .await
    }

    async fn fs_search(&self, p: FsSearchParams) -> CoreResult<FsSearchResult> {
        let root = match (&p.thread_id, &p.project_id) {
            (Some(t), _) => {
                let id = t.clone();
                let row = self
                    .sh
                    .db
                    .read(move |tx| store::get_thread(tx, &id))
                    .await?
                    .ok_or_else(|| not_found("thread", t))?;
                PathBuf::from(row.cwd)
            }
            (None, Some(pid)) => PathBuf::from(self.project(pid).await?.path),
            (None, None) => return Err(invalid_params("projectId or threadId is required")),
        };
        let limit = p
            .limit
            .map(|l| l as usize)
            .unwrap_or(self.sh.config.heuristics.file_search_max_results)
            .min(self.sh.config.policy.max_file_search_results);
        let sh = self.sh.clone();
        let query = p.query;
        let results = tokio::task::spawn_blocking(move || sh.fs.search(&root, &query, limit))
            .await
            .map_err(|e| CoreError::Internal(e.to_string()))?;
        Ok(FsSearchResult {
            results,
            ranking: "heuristic:H1".into(),
        })
    }

    // ----- threads -----------------------------------------------------------------------------

    async fn thread_view(&self, id: &ThreadId) -> CoreResult<Thread> {
        let key = id.clone();
        self.sh
            .db
            .read(move |tx| {
                let row = store::get_thread(tx, &key)?
                    .filter(|r| !r.removed)
                    .ok_or_else(|| not_found("thread", &key))?;
                store::thread_view(tx, &row)
            })
            .await
    }

    async fn thread_list(&self, p: ThreadListParams) -> CoreResult<ThreadListResult> {
        let policy = &self.sh.config.policy;
        let limit = p
            .limit
            .map(|l| l as usize)
            .unwrap_or(policy.thread_list_default_limit)
            .clamp(1, policy.thread_list_max_limit);
        self.sh
            .db
            .read(move |tx| {
                let (rows, has_more) = store::list_threads(
                    tx,
                    p.project_id.as_ref(),
                    p.include_archived,
                    limit,
                    p.before.as_ref(),
                )?;
                let threads = rows
                    .iter()
                    .map(|r| store::thread_view(tx, r))
                    .collect::<CoreResult<Vec<_>>>()?;
                Ok(ThreadListResult { threads, has_more })
            })
            .await
    }

    fn validate_new_settings(
        &self,
        harness: &str,
        settings: &ThreadSettings,
    ) -> CoreResult<HarnessInfo> {
        let info = self
            .sh
            .registry
            .info(harness)
            .ok_or_else(|| invalid_params(format!("unknown harness {harness}")))?;
        if !info.available {
            return Err(unavailable_error(harness, Some(&info)));
        }
        actor::validate_settings(&info, settings)?;
        Ok(info)
    }

    async fn thread_create(
        self: &Arc<Self>,
        p: ThreadCreateParams,
        idem: Option<Idem>,
    ) -> CoreResult<ThreadCreateResult> {
        self.sh.registry.wait_ready().await;
        let project = self.project(&p.project_id).await?;
        if self.sh.registry.get(&p.harness_id).is_none() {
            return Err(invalid_params(format!("unknown harness {}", p.harness_id)));
        }
        self.sh.recheck_unavailable_harness(&p.harness_id).await;
        let info_settings = {
            let d = &project.defaults;
            let s = p.settings.clone().unwrap_or_default();
            let from_defaults = d.harness_id.as_deref() == Some(p.harness_id.as_str());
            ThreadSettings {
                model: s
                    .model
                    .or_else(|| if from_defaults { d.model.clone() } else { None }),
                effort: s.effort.or_else(|| {
                    if from_defaults {
                        d.effort.clone()
                    } else {
                        None
                    }
                }),
                permission_mode: s.permission_mode.or_else(|| {
                    if from_defaults {
                        d.permission_mode.clone()
                    } else {
                        None
                    }
                }),
            }
        };
        let info = self.validate_new_settings(&p.harness_id, &info_settings)?;
        let settings = ThreadSettings {
            model: info_settings.model.or(info.default_model.clone()),
            effort: info_settings.effort,
            permission_mode: info_settings
                .permission_mode
                .or(info.default_permission_mode.clone()),
        };
        let thread_id = ThreadId::generate();
        let input = p.input.filter(|i| !i.is_empty());
        if let Some(input) = &input {
            // The first turn is checked before anything is created on disk: a refused turn
            // stores no thread, and must not leave a worktree and a branch behind. (Mentions
            // are relative paths, so the project's folder serves for checking them.)
            if !self.sh.accepts_work() {
                return Err(rpc(ErrorKind::Draining, "the server is shutting down"));
            }
            actor::check_input(
                self.sh.clone(),
                PathBuf::from(&project.path),
                input.clone(),
                info.capabilities.images,
            )
            .await?;
        }
        let mut created_worktree = None;
        let (cwd, workspace) = match p.workspace.clone().unwrap_or_default() {
            WorkspaceSpec::Local => (project.path.clone(), Workspace::Local),
            WorkspaceSpec::Worktree { base_ref, branch } => {
                let git = self
                    .sh
                    .git
                    .clone()
                    .ok_or_else(|| invalid_state("git is not available on this PC"))?;
                if !project.git.is_repo {
                    return Err(invalid_state("worktrees need a git repository"));
                }
                let suffix = thread_id
                    .as_str()
                    .rsplit('_')
                    .next()
                    .unwrap_or_default()
                    .to_lowercase();
                let short: String = suffix
                    .chars()
                    .rev()
                    .take(WORKTREE_BRANCH_ID_CHARS)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect();
                let branch = branch.unwrap_or_else(|| format!("aas/{short}"));
                if branch.starts_with('-') {
                    return Err(invalid_params(format!("invalid branch name {branch:?}")));
                }
                validate_name(branch.split('/').next_back().unwrap_or(&branch))?;
                let base = base_ref.unwrap_or_else(|| "HEAD".into());
                // Passed to git as a name only: something that looks like an option is refused
                // (and `--end-of-options` guards the calls anyway), and the name must resolve
                // to a commit before a worktree or a branch is created.
                if base.trim().is_empty() || base.starts_with('-') {
                    return Err(invalid_params(format!(
                        "baseRef {base:?} is not a revision name"
                    )));
                }
                if git
                    .resolve_commit(Path::new(&project.path), &base)
                    .await?
                    .is_none()
                {
                    return Err(invalid_params(format!(
                        "baseRef {base:?} does not name a commit of {}",
                        project.path
                    )));
                }
                let path = self
                    .sh
                    .config
                    .worktrees_dir()
                    .join(project.id.as_str())
                    .join(thread_id.as_str());
                git.worktree_add(Path::new(&project.path), &path, &branch, &base)
                    .await?;
                created_worktree = Some((path.clone(), branch.clone()));
                (
                    path.display().to_string(),
                    Workspace::Worktree {
                        path: path.display().to_string(),
                        branch,
                        base_ref: base,
                    },
                )
            }
        };
        let result = self
            .insert_new_thread(
                &thread_id,
                &project,
                p.harness_id,
                p.title,
                cwd,
                workspace,
                settings,
                input,
                idem,
            )
            .await;
        if let (Err(e), Some((path, branch))) = (&result, created_worktree) {
            // The thread was not stored (a race with the checks above: a drain that began, a
            // blob collected meanwhile, or a failed write): nothing refers to its worktree.
            tracing::info!(thread = %thread_id, error = %e, "the new thread was not created; discarding its worktree");
            self.discard_new_worktree(Path::new(&project.path), &path, &branch)
                .await;
        }
        result
    }

    /// The part of [`thread_create`](Self::thread_create) after the workspace exists: stores
    /// the thread, with its first turn when `input` is given.
    // Private continuation of `thread_create` taking its already-validated parameters one to
    // one; a struct for this single call site would only duplicate the request type.
    #[allow(clippy::too_many_arguments)]
    async fn insert_new_thread(
        self: &Arc<Self>,
        thread_id: &ThreadId,
        project: &Project,
        harness_id: String,
        title: Option<String>,
        cwd: String,
        workspace: Workspace,
        settings: ThreadSettings,
        input: Option<Vec<InputPart>>,
        idem: Option<Idem>,
    ) -> CoreResult<ThreadCreateResult> {
        let thread_id = thread_id.clone();
        let now = now_ms();
        let (title, title_source) = match title.as_deref().map(str::trim) {
            Some(t) if !t.is_empty() => (t.to_owned(), "user"),
            _ => ("New thread".to_owned(), "default"),
        };
        let row = ThreadRow {
            id: thread_id.clone(),
            project_id: project.id.clone(),
            harness_id,
            title,
            title_source: title_source.into(),
            cwd,
            workspace,
            settings,
            status: ThreadStatus::Idle,
            native_session_id: None,
            fork_source: None,
            forked_from: None,
            last_error: None,
            usage: Usage::default(),
            base_tree: None,
            diff_available: false,
            queue_paused: false,
            head: 0,
            created_at: now,
            updated_at: now,
            last_activity_at: now,
            archived: false,
            removed: false,
            pinned: false,
        };
        match input {
            Some(input) => {
                let handle = actor::spawn(self.sh.clone(), row, false, 0, 0, None);
                self.actors.lock().insert(thread_id.clone(), handle.clone());
                let (tx, rx) = oneshot::channel();
                handle
                    .send(Msg::StartTurn {
                        input,
                        delivery: Delivery::Auto,
                        idem,
                        create: true,
                        reply: tx,
                    })
                    .map_err(|_| CoreError::Internal("thread actor is gone".into()))?;
                let started = rx
                    .await
                    .map_err(|_| CoreError::Internal("thread actor dropped the request".into()))?;
                match started {
                    Ok(r) => Ok(ThreadCreateResult {
                        thread: self.thread_view(&thread_id).await?,
                        turn_id: r.turn_id,
                        disposition: Some(r.disposition),
                    }),
                    Err(e) => {
                        self.actors.lock().remove(&thread_id);
                        Err(CoreError::Rpc(e))
                    }
                }
            }
            None => {
                let thread = self
                    .sh
                    .tx(move |tx, em| {
                        let mut row = row;
                        store::insert_thread(tx, &row)?;
                        let view = thread_changed(tx, em, &mut row)?;
                        let result = ThreadCreateResult {
                            thread: view,
                            turn_id: None,
                            disposition: None,
                        };
                        if let Some(idem) = &idem {
                            idem.store_result(tx, &result)?;
                        }
                        Ok(result)
                    })
                    .await?;
                Ok(thread)
            }
        }
    }

    /// Removes the worktree and the branch `thread/create` made for a thread that was not
    /// stored. Nothing ran in it, so the removal is forced (git's own files cannot stop it).
    /// What fails becomes cleanup jobs, retried by maintenance until they succeed.
    async fn discard_new_worktree(&self, repo: &Path, path: &Path, branch: &str) {
        let Some(git) = &self.sh.git else { return };
        let removed = match git.worktree_remove(repo, path, true).await {
            Ok(()) => git.branch_delete(repo, branch).await,
            Err(e) => Err(e),
        };
        match removed {
            Ok(()) => retention::remove_empty_parent(path, &self.sh.config.worktrees_dir()),
            Err(e) => {
                tracing::warn!(worktree = %path.display(), branch, error = %e, "could not discard the worktree of a thread that was not created; retried by maintenance");
                let (repo, path, branch) = (
                    repo.display().to_string(),
                    path.display().to_string(),
                    branch.to_owned(),
                );
                let recorded = self
                    .sh
                    .db
                    .write(move |tx| {
                        let now = now_ms();
                        store::insert_cleanup_job(tx, CleanupKind::Worktree, &repo, &path, now)?;
                        store::insert_cleanup_job(tx, CleanupKind::Branch, &repo, &branch, now)?;
                        Ok(())
                    })
                    .await;
                if let Err(e) = recorded {
                    tracing::error!(error = %e, "could not record the cleanup of a discarded worktree; it stays until removed by hand");
                }
            }
        }
    }

    async fn thread_read(&self, p: ThreadReadParams) -> CoreResult<ThreadReadResult> {
        let policy = &self.sh.config.policy;
        let limit = p
            .limit_turns
            .map(|l| l as usize)
            .unwrap_or(policy.thread_read_default_turns)
            .clamp(1, policy.thread_read_max_turns);
        let preview_chars = policy.queued_preview_chars;
        self.sh
            .db
            .read(move |tx| {
                let row = store::get_thread(tx, &p.thread_id)?
                    .filter(|r| !r.removed)
                    .ok_or_else(|| not_found("thread", &p.thread_id))?;
                let thread = store::thread_view(tx, &row)?;
                let (turn_rows, has_more_before) =
                    store::list_turns(tx, &p.thread_id, p.before_turn_index, limit)?;
                let ids: Vec<TurnId> = turn_rows.iter().map(|t| t.turn.id.clone()).collect();
                let items = store::items_of_turns(tx, &ids)?;
                let interactions = store::interactions_for_read(tx, &p.thread_id, &ids)?;
                let queued = store::list_queued(tx, &p.thread_id, preview_chars)?;
                let background_tasks = store::background_tasks_for_read(tx, &p.thread_id, &ids)?;
                let head = aas_eventlog::head(tx, &thread_stream(&p.thread_id))?;
                Ok(ThreadReadResult {
                    thread,
                    turns: turn_rows.into_iter().map(|t| t.turn).collect(),
                    items,
                    interactions,
                    queued,
                    background_tasks,
                    head,
                    has_more_before,
                })
            })
            .await
    }

    async fn thread_fork(
        &self,
        p: ThreadForkParams,
        idem: Option<Idem>,
    ) -> CoreResult<ThreadResult> {
        let pid = p.thread_id.clone();
        let (parent, turns) = self
            .sh
            .db
            .read(move |tx| {
                let parent = store::get_thread(tx, &pid)?
                    .filter(|r| !r.removed)
                    .ok_or_else(|| not_found("thread", &pid))?;
                let turns = store::all_turns(tx, &pid)?;
                Ok((parent, turns))
            })
            .await?;
        self.sh.registry.wait_ready().await;
        if self.sh.registry.get(&parent.harness_id).is_none() {
            return Err(invalid_state("harness not configured"));
        }
        self.sh
            .recheck_unavailable_harness(&parent.harness_id)
            .await;
        let info = self.available_info(&parent.harness_id)?;
        if !info.capabilities.fork {
            return Err(rpc(
                ErrorKind::CapabilityUnsupported,
                "this harness cannot fork sessions",
            )
            .with_cap("fork"));
        }
        let Some(native) = parent.native_session_id.clone() else {
            return Err(invalid_state("the thread has no agent session to fork yet"));
        };
        if turns.iter().any(|t| t.turn.status == TurnStatus::Running) {
            return Err(invalid_state(
                "wait for the running turn to finish before forking",
            ));
        }
        if let Some(at) = &p.at_turn_id
            && turns.last().map(|t| &t.turn.id) != Some(at)
        {
            return Err(rpc(
                ErrorKind::CapabilityUnsupported,
                "forking at an earlier turn is not supported",
            )
            .with_cap("forkAtTurn"));
        }
        let now = now_ms();
        let new_id = ThreadId::generate();
        let row = ThreadRow {
            id: new_id.clone(),
            title: format!("Fork of {}", parent.title),
            title_source: "fork".into(),
            status: ThreadStatus::Idle,
            native_session_id: None,
            fork_source: Some(native),
            forked_from: Some(ForkOrigin {
                thread_id: parent.id.clone(),
                turn_id: turns.last().map(|t| t.turn.id.clone()),
            }),
            last_error: None,
            queue_paused: false,
            head: 0,
            created_at: now,
            updated_at: now,
            last_activity_at: now,
            archived: false,
            removed: false,
            pinned: false,
            ..parent.clone()
        };
        // The copied turns refer to the snapshots of the parent: keep them for the fork too, so
        // they survive the removal of the parent.
        if let Some(git) = &self.sh.git {
            let trees: Vec<String> = turns
                .iter()
                .flat_map(|t| [t.base_tree.clone(), t.end_tree.clone()])
                .chain([row.base_tree.clone()])
                .flatten()
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect();
            git.keep(Path::new(&row.cwd), new_id.as_str(), &trees).await;
        }
        self.sh
            .tx(move |tx, em| {
                let mut row = row;
                store::insert_thread(tx, &row)?;
                // Copy the visible history so the fork shows its context.
                let ids: Vec<TurnId> = turns.iter().map(|t| t.turn.id.clone()).collect();
                let items = store::items_of_turns(tx, &ids)?;
                let mut turn_map = HashMap::new();
                for t in &turns {
                    let mut copy = t.clone();
                    copy.turn.id = TurnId::generate();
                    copy.turn.thread_id = row.id.clone();
                    turn_map.insert(t.turn.id.clone(), copy.turn.id.clone());
                    store::insert_turn(tx, &copy)?;
                }
                for mut item in items {
                    item.id = ItemId::generate();
                    item.thread_id = row.id.clone();
                    item.turn_id = turn_map.get(&item.turn_id).cloned().unwrap_or(item.turn_id);
                    // Background tasks are not copied: they belong to the parent's process.
                    item.background_task_id = None;
                    store::insert_item(tx, &item)?;
                }
                let view = thread_changed(tx, em, &mut row)?;
                let result = ThreadResult { thread: view };
                if let Some(idem) = &idem {
                    idem.store_result(tx, &result)?;
                }
                Ok(result)
            })
            .await
    }

    async fn thread_diff(&self, p: ThreadDiffParams) -> CoreResult<ThreadDiffResult> {
        let git = self
            .sh
            .git
            .clone()
            .ok_or_else(|| invalid_state("git is not available on this PC"))?;
        let pid = p.thread_id.clone();
        let scope = p.scope.clone();
        let (row, turn) = self
            .sh
            .db
            .read(move |tx| {
                let row = store::get_thread(tx, &pid)?
                    .filter(|r| !r.removed)
                    .ok_or_else(|| not_found("thread", &pid))?;
                let turn = match &scope {
                    DiffScope::Turn { turn_id } => {
                        let t = store::get_turn(tx, turn_id)?
                            .filter(|t| t.turn.thread_id == pid)
                            .ok_or_else(|| not_found("turn", turn_id))?;
                        Some(t)
                    }
                    DiffScope::Thread => None,
                };
                Ok((row, turn))
            })
            .await?;
        let cwd = PathBuf::from(&row.cwd);
        let live_snapshot = || async {
            git.snapshot_tree(&cwd, None)
                .await?
                .ok_or_else(|| invalid_state("the folder is no longer a git repository"))
        };
        let (base, head) = match turn {
            Some(TurnRow {
                base_tree: Some(base),
                end_tree,
                turn,
            }) => {
                let head = match end_tree {
                    Some(h) => h,
                    // Still running: its changes so far.
                    None if turn.status == TurnStatus::Running => live_snapshot().await?,
                    None => {
                        return Err(invalid_state(
                            "the end of this turn was not recorded (no snapshot could be taken)",
                        ));
                    }
                };
                (base, head)
            }
            Some(_) => {
                return Err(invalid_state(
                    "this turn has no recorded snapshot (not a git repository at the time)",
                ));
            }
            None => {
                let base = row
                    .base_tree
                    .clone()
                    .ok_or_else(|| invalid_state("this thread has no recorded snapshot yet"))?;
                (base, live_snapshot().await?)
            }
        };
        for tree in [&base, &head] {
            if !git.has_tree(&cwd, tree).await? {
                return Err(invalid_state(format!(
                    "snapshot {tree} is no longer in the repository (removed by git gc); this diff cannot be shown"
                )));
            }
        }
        let files = git.diff_files(&cwd, &base, &head).await?;
        let patch = git.diff_patch(&cwd, &base, &head).await?;
        let summary = DiffSummary {
            files: files.len() as u32,
            insertions: files.iter().map(|f| f.added).sum(),
            deletions: files.iter().map(|f| f.removed).sum(),
        };
        if patch.len() <= self.sh.config.policy.max_inline_patch_bytes {
            Ok(ThreadDiffResult {
                summary,
                files,
                patch: Some(patch),
                patch_blob_id: None,
            })
        } else {
            let store = self.sh.blobs.clone();
            let bytes = patch.into_bytes();
            let (id, size, pin) = tokio::task::spawn_blocking(move || store.put_bytes(&bytes))
                .await
                .map_err(|e| CoreError::Internal(e.to_string()))??;
            // Produced for the client to download: nothing refers to it, so it is kept for
            // `unreferenced_blob_grace` (producing the same patch again restarts that).
            let key = id.clone();
            self.sh
                .db
                .write(move |tx| {
                    store::insert_blob(tx, &key, "text/x-diff; charset=utf-8", size, now_ms())
                })
                .await?;
            drop(pin);
            Ok(ThreadDiffResult {
                summary,
                files,
                patch: None,
                patch_blob_id: Some(id),
            })
        }
    }

    // ----- commands, native sessions ----------------------------------------------------------

    async fn command_list(&self, p: CommandListParams) -> CoreResult<CommandListResult> {
        self.sh.registry.wait_ready().await;
        let (harness_id, cwd, native, thread) = match (&p.thread_id, &p.project_id, &p.harness_id) {
            (Some(t), _, _) => {
                let id = t.clone();
                let row = self
                    .sh
                    .db
                    .read(move |tx| store::get_thread(tx, &id))
                    .await?
                    .filter(|r| !r.removed)
                    .ok_or_else(|| not_found("thread", t))?;
                (
                    row.harness_id.clone(),
                    PathBuf::from(&row.cwd),
                    row.native_session_id.clone(),
                    Some(row.id),
                )
            }
            (None, Some(project), Some(h)) => (
                h.clone(),
                PathBuf::from(self.project(project).await?.path),
                None,
                None,
            ),
            _ => {
                return Err(invalid_params(
                    "threadId, or projectId and harnessId, are required",
                ));
            }
        };
        let adapter = self
            .sh
            .registry
            .get(&harness_id)
            .ok_or_else(|| not_found("harness", &harness_id))?;
        let info = self
            .sh
            .registry
            .info(&harness_id)
            .unwrap_or_else(|| HarnessInfo::unavailable("not probed"));
        let mut commands = app_commands(&info, thread.is_some());
        let cached = match &thread {
            Some(id) => {
                let handle = self.actors.lock().get(id).cloned();
                match handle {
                    Some(h) => {
                        let (tx, rx) = oneshot::channel();
                        if h.send(Msg::Commands { reply: tx }).is_ok() {
                            rx.await.ok().flatten()
                        } else {
                            None
                        }
                    }
                    None => None,
                }
            }
            None => None,
        };
        let harness_commands = match cached {
            Some(c) => c,
            None if info.available => adapter
                .commands(CommandContext { cwd, native_session_id: native })
                .await
                .unwrap_or_else(|e| {
                    tracing::warn!(harness = %harness_id, error = %e, "listing harness commands failed");
                    Vec::new()
                }),
            None => Vec::new(),
        };
        let reserved = app_commands(&info, true);
        let switching = adapter.session_switching_commands();
        commands.extend(harness_commands.into_iter().filter(|c| {
            !commands_contains(&reserved, &c.name)
                && !is_session_switching_command(&c.name, switching)
        }));
        Ok(CommandListResult { commands })
    }

    async fn native_list(&self, p: NativeListParams) -> CoreResult<NativeListResult> {
        self.sh.registry.wait_ready().await;
        let project = self.project(&p.project_id).await?;
        let adapter = self
            .sh
            .registry
            .get(&p.harness_id)
            .ok_or_else(|| not_found("harness", &p.harness_id))?;
        self.sh.recheck_unavailable_harness(&p.harness_id).await;
        let info = self.available_info(&p.harness_id)?;
        if !info.capabilities.native_sessions {
            return Err(rpc(
                ErrorKind::CapabilityUnsupported,
                "this harness cannot list its sessions",
            )
            .with_cap("nativeSessions"));
        }
        let listed = adapter
            .list_native_sessions(Path::new(&project.path))
            .await
            .map_err(|e| rpc(ErrorKind::AdapterError, e.to_string()))?;
        let sessions = unique_native_sessions(&p.harness_id, listed);
        let harness = p.harness_id.clone();
        let sessions = self
            .sh
            .db
            .read(move |tx| {
                sessions
                    .into_iter()
                    .map(|s| {
                        Ok(NativeSession {
                            imported_thread_id: store::thread_by_native_session(
                                tx,
                                &harness,
                                &s.native_session_id,
                            )?,
                            native_session_id: s.native_session_id,
                            title: s.title,
                            updated_at: s.updated_at,
                            cwd: s.cwd,
                        })
                    })
                    .collect::<CoreResult<Vec<_>>>()
            })
            .await?;
        Ok(NativeListResult { sessions })
    }

    async fn native_import(
        &self,
        p: NativeImportParams,
        idem: Option<Idem>,
    ) -> CoreResult<ThreadResult> {
        self.sh.registry.wait_ready().await;
        let project = self.project(&p.project_id).await?;
        let adapter = self
            .sh
            .registry
            .get(&p.harness_id)
            .ok_or_else(|| not_found("harness", &p.harness_id))?;
        self.sh.recheck_unavailable_harness(&p.harness_id).await;
        let info = self.available_info(&p.harness_id)?;
        if !info.capabilities.native_sessions {
            return Err(rpc(
                ErrorKind::CapabilityUnsupported,
                "this harness cannot import sessions",
            )
            .with_cap("nativeSessions"));
        }
        let (h, n) = (p.harness_id.clone(), p.native_session_id.clone());
        if let Some(existing) = self
            .sh
            .db
            .read(move |tx| store::thread_by_native_session(tx, &h, &n))
            .await?
        {
            return Ok(ThreadResult {
                thread: self.thread_view(&existing).await?,
            });
        }
        let history = adapter
            .read_native_history(Path::new(&project.path), &p.native_session_id)
            .await
            .map_err(|e| rpc(ErrorKind::AdapterError, e.to_string()))?;
        let now = now_ms();
        let title = history
            .title
            .clone()
            .unwrap_or_else(|| "Imported session".into());
        let row = ThreadRow {
            id: ThreadId::generate(),
            project_id: project.id.clone(),
            harness_id: p.harness_id.clone(),
            title,
            title_source: "import".into(),
            cwd: project.path.clone(),
            workspace: Workspace::Local,
            settings: ThreadSettings {
                model: info.default_model.clone(),
                effort: None,
                permission_mode: info.default_permission_mode.clone(),
            },
            status: ThreadStatus::Idle,
            native_session_id: Some(p.native_session_id.clone()),
            fork_source: None,
            forked_from: None,
            last_error: None,
            usage: Usage::default(),
            base_tree: None,
            diff_available: false,
            queue_paused: false,
            head: 0,
            created_at: now,
            updated_at: now,
            last_activity_at: history
                .turns
                .last()
                .and_then(|t| t.completed_at.or(t.started_at))
                .unwrap_or(now),
            archived: false,
            removed: false,
            pinned: false,
        };
        let (harness, native) = (p.harness_id.clone(), p.native_session_id.clone());
        self.sh
            .tx(move |tx, em| {
                // Checked again in the writing transaction: a concurrent import of the same
                // session (a double tap, another device) may have finished meanwhile.
                if let Some(existing) = store::thread_by_native_session(tx, &harness, &native)? {
                    let row = store::get_thread(tx, &existing)?
                        .ok_or_else(|| not_found("thread", &existing))?;
                    let result = ThreadResult {
                        thread: store::thread_view(tx, &row)?,
                    };
                    if let Some(idem) = &idem {
                        idem.store_result(tx, &result)?;
                    }
                    return Ok(result);
                }
                let mut row = row;
                store::insert_thread(tx, &row)?;
                for (index, t) in history.turns.into_iter().enumerate() {
                    let started = t.started_at.unwrap_or(now);
                    let turn = Turn {
                        id: TurnId::generate(),
                        thread_id: row.id.clone(),
                        index: index as u32,
                        status: TurnStatus::Completed,
                        started_at: started,
                        completed_at: Some(t.completed_at.unwrap_or(started)),
                        model: None,
                        error: None,
                        usage: None,
                        diff: None,
                        trigger: None,
                    };
                    store::insert_turn(
                        tx,
                        &TurnRow {
                            turn: turn.clone(),
                            base_tree: None,
                            end_tree: None,
                        },
                    )?;
                    for hi in t.items {
                        let item = Item {
                            id: ItemId::generate(),
                            thread_id: row.id.clone(),
                            turn_id: turn.id.clone(),
                            status: hi.status,
                            started_at: started,
                            completed_at: Some(turn.completed_at.unwrap_or(started)),
                            background_task_id: None,
                            body: hi.body,
                        };
                        store::insert_item(tx, &item)?;
                    }
                }
                let view = thread_changed(tx, em, &mut row)?;
                let result = ThreadResult { thread: view };
                if let Some(idem) = &idem {
                    idem.store_result(tx, &result)?;
                }
                Ok(result)
            })
            .await
    }

    // ----- lifecycle ---------------------------------------------------------------------------

    /// One maintenance pass now (it also runs every `policy.maintenance_interval`): event
    /// compaction, expiry of records, blob collection, cleanup jobs and space reclamation
    /// (design.md §6.1).
    pub async fn run_maintenance(&self) -> CoreResult<MaintenanceReport> {
        let report = retention::run(&self.sh).await;
        self.sh.hub.prune();
        self.actors.lock().retain(|_, h| !h.is_closed());
        if let Ok(r) = &report {
            tracing::debug!(report = ?r, "maintenance done");
        }
        report
    }

    /// Pages of the database file and how many of them are free (diagnostics).
    pub async fn database_pages(&self) -> CoreResult<(u64, u64)> {
        self.sh.db.page_counts().await
    }

    /// Stops accepting new turns and resolves once no turn runs and no background work keeps
    /// an agent busy any more, both at the same moment (a run the agent started by itself
    /// counts as a running turn once its start is reported).
    ///
    /// A run the agent is about to start because its background work ended is not waited for:
    /// Claude Code reports the end of the work before that run's start, and nothing explicit
    /// in between says a run will follow (design.md §1 out of scope, §18.5). Such a run gets
    /// the staged stop's grace after the drain (`policy.stop_grace`, stdin closed first).
    ///
    /// Cancel safe: the caller may stop waiting (e.g. when a drain is escalated to an immediate
    /// stop).
    pub async fn wait_drained(&self) {
        self.sh.draining.store(true, Ordering::SeqCst);
        let mut turns = self.sh.running_turns.subscribe();
        let mut background = self.sh.running_background.subscribe();
        loop {
            let busy_turns = *turns.borrow_and_update();
            let busy_background = *background.borrow_and_update();
            if busy_turns == 0 && busy_background == 0 {
                return;
            }
            tokio::select! {
                changed = turns.changed() => if changed.is_err() { return },
                changed = background.changed() => if changed.is_err() { return },
            }
        }
    }

    /// Stops accepting turns (and waits for running ones when `drain`), stops every agent
    /// process and ends all actors. Idempotent: a concurrent or later call returns once the
    /// first one has stopped everything (the fail-stop and the daemon both call it).
    pub async fn shutdown(&self, drain: bool) {
        self.sh.draining.store(true, Ordering::SeqCst);
        if drain {
            self.wait_drained().await;
        }
        let mut done = self.shutdown_done.lock().await;
        if *done {
            return;
        }
        // Running clones are stopped (recorded as failed: the daemon stopped) while the agents
        // stop, so no tool outlives the engine and neither waits for the other.
        let handles: Vec<ActorHandle> = self.actors.lock().drain().map(|(_, h)| h).collect();
        let mut waits = Vec::new();
        for h in handles {
            let (tx, rx) = oneshot::channel();
            if h.send(Msg::Shutdown { reply: tx }).is_ok() {
                waits.push(rx);
            }
        }
        tokio::join!(
            self.sh.operations.shutdown(),
            futures::future::join_all(waits)
        );
        self.stopped.store(true, Ordering::SeqCst);
        *done = true;
    }

    /// [`shutdown`](Self::shutdown) without a drain because Windows ends the session
    /// (sign-out, shutdown, reboot): the turns it ends are recorded with the error kind
    /// `systemShutdown`, so the app can tell them from a daemon that was stopped or restarted.
    ///
    /// Waits at most `policy.end_session_stop_grace` for the agents' staged stop (their stdin
    /// is closed and each CLI saves its session); what is still running then ends with the
    /// daemon's Job Objects when the daemon exits right after. The end of the session is
    /// recorded first, so that the next start records the turns that could not be recorded in
    /// time as `systemShutdown` too (not `daemonRestarted`).
    pub async fn shutdown_for_end_session(&self) {
        self.sh.session_ending.store(true, Ordering::SeqCst);
        let grace = self.sh.config.policy.end_session_stop_grace;
        let noted = self
            .sh
            .db
            .write(|tx| store::meta_set(tx, store::META_SESSION_ENDED_AT, &now_ms().to_string()));
        let (noted, stopped) = tokio::join!(
            tokio::time::timeout(grace, noted),
            tokio::time::timeout(grace, self.shutdown(false))
        );
        match noted {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "could not record the end of the session; turns not recorded in time become daemonRestarted")
            }
            Err(_) => tracing::warn!(
                ?grace,
                "recording the end of the session did not finish within policy.end_session_stop_grace"
            ),
        }
        if stopped.is_err() {
            tracing::warn!(
                ?grace,
                "agents are still stopping at policy.end_session_stop_grace; the rest ends with the daemon's Job Objects"
            );
        }
    }

    /// Closes the database after [`shutdown`](Self::shutdown), releasing its files (waits for
    /// reads and writes in progress). The engine answers nothing but errors afterwards; the
    /// test server uses this to delete or reopen the database within one process.
    pub async fn close(&self) -> CoreResult<()> {
        self.sh.db.close().await
    }

    /// Probes harness `id` (all of them for `None`) again and publishes the results as
    /// `harness/updated` (`harness/refresh`, and `agent-app-server harness refresh` through
    /// the admin API). Waits for the probes and returns every harness. A probe that started
    /// after this call is used instead of starting another one.
    pub async fn refresh_harnesses(&self, id: Option<&str>) -> CoreResult<Vec<Harness>> {
        let asked = tokio::time::Instant::now();
        let targets = match id {
            Some(id) if self.sh.registry.get(id).is_none() => {
                return Err(not_found("harness", id));
            }
            Some(id) => vec![id.to_owned()],
            None => self.sh.registry.ids(),
        };
        let probes: Vec<_> = targets
            .into_iter()
            .map(|id| self.sh.probe_harness(id, Some(asked), Publish::Always))
            .collect();
        for probe in futures::future::join_all(probes).await {
            probe.map_err(|e| CoreError::Internal(format!("the probe task failed: {e}")))?;
        }
        Ok(self.sh.registry.harnesses())
    }

    /// The information of harness `id`, or `harnessUnavailable` when its last probe found it
    /// unavailable.
    fn available_info(&self, id: &str) -> CoreResult<HarnessInfo> {
        match self.sh.registry.info(id) {
            Some(info) if info.available => Ok(info),
            other => Err(unavailable_error(id, other.as_ref())),
        }
    }

    /// The write failpoint of the engine's database (tests of this crate).
    #[cfg(test)]
    pub(crate) fn failpoint(&self) -> &crate::db::WriteFailpoint {
        self.sh.db.failpoint()
    }

    /// Database integrity check (for `doctor`).
    pub async fn check_database(&self) -> CoreResult<String> {
        self.sh.db.quick_check().await
    }
}

/// The error message of a turn ended because Windows ended the session.
pub(crate) const SYSTEM_SHUTDOWN_MESSAGE: &str =
    "Windows is signing out, shutting down or restarting";

/// Outcome of one actor lookup (see [`Engine::actor`]).
enum Lookup {
    Found(ActorHandle),
    /// The thread is being removed: wait until that is decided.
    Removing,
    /// A removal started or ended during the lookup: look again.
    Retry,
}

fn to_value<T: serde::Serialize>(v: T) -> CoreResult<Value> {
    Ok(serde_json::to_value(v)?)
}

fn with_git(mut project: Project) -> Project {
    project.git = quick_info(Path::new(&project.path));
    project
}

/// Registers the folder `path` as a project inside `tx`: the existing project of that folder
/// (also a removed or archived one) is brought back, otherwise a new one is inserted.
pub(crate) fn upsert_project(
    tx: &rusqlite::Transaction<'_>,
    em: &mut Emitter,
    path: &Path,
    name: String,
) -> CoreResult<Project> {
    let key = path_key(path);
    let now = now_ms();
    let project = match store::find_project_by_key(tx, &key)? {
        Some(mut existing) => {
            existing.project.archived = false;
            existing.project.updated_at = now;
            store::update_project(tx, &existing.project, false)?;
            existing.project
        }
        None => {
            let p = Project {
                id: ProjectId::generate(),
                name,
                path: path.display().to_string(),
                created_at: now,
                updated_at: now,
                archived: false,
                defaults: ProjectDefaults::default(),
                git: GitInfo::default(),
            };
            store::insert_project(tx, &p, &key)?;
            p
        }
    };
    let project = with_git(project);
    em.workspace(Event::ProjectUpserted {
        project: project.clone(),
    });
    Ok(project)
}

fn commands_contains(list: &[Command], name: &str) -> bool {
    list.iter().any(|c| c.name == name)
}

/// Harness command names that are never offered, whatever the harness.
///
/// `resume`: every CLI that has a command of that name uses it to open another of its sessions
/// in the running process (Claude Code's and pi's session pickers, Codex's `/resume`). A thread
/// is one native session (design.md §9.5), and the app offers its own `/resume` that opens the
/// session import (`native/list`, `native/import`), which a harness command of the same name
/// would hide (docs/ux/codex-desktop.md §8.5).
const SESSION_SWITCHING_COMMAND_NAMES: &[&str] = &["resume"];

/// Whether a harness command switches the native session inside the running process: one of
/// [`SESSION_SWITCHING_COMMAND_NAMES`], or one the adapter names
/// ([`aas_harness::HarnessAdapter::session_switching_commands`]).
fn is_session_switching_command(name: &str, adapter_names: &[&str]) -> bool {
    SESSION_SWITCHING_COMMAND_NAMES.contains(&name) || adapter_names.contains(&name)
}

/// `native/list` names each native session once (protocol.md, `NativeSession`): a thread
/// imports one native session, and clients key their lists by `nativeSessionId`. Entries that
/// repeat an id are merged, keeping the position of the first and the content of the latest
/// (`updatedAt`). Adapters merge what their CLI repeats, so a repeat reaching the engine is an
/// adapter defect and is logged.
fn unique_native_sessions(
    harness_id: &str,
    listed: Vec<aas_harness::NativeSessionSummary>,
) -> Vec<aas_harness::NativeSessionSummary> {
    let set: aas_harness::NativeSessionSet = listed.into_iter().collect();
    if set.repeated() > 0 {
        tracing::warn!(
            harness = harness_id,
            dropped = set.repeated(),
            "the adapter listed native sessions more than once; kept the latest entry of each"
        );
    }
    set.into_sessions()
}

/// Commands the app implements itself (mapped to protocol methods or pickers).
fn app_commands(info: &HarnessInfo, in_thread: bool) -> Vec<Command> {
    let cmd = |name: &str, description: &str, action: CommandAction| Command {
        name: name.into(),
        description: Some(description.into()),
        source: CommandSource::App,
        argument_hint: None,
        action,
    };
    let mut out = Vec::new();
    if !info.models.is_empty() {
        out.push(cmd(
            "model",
            "Choose the model",
            CommandAction::Picker {
                picker: PickerKind::Model,
            },
        ));
    }
    if !info.effort_levels.is_empty() {
        out.push(cmd(
            "effort",
            "Choose the reasoning effort",
            CommandAction::Picker {
                picker: PickerKind::Effort,
            },
        ));
    }
    if !info.permission_modes.is_empty() {
        out.push(cmd(
            "permissions",
            "Choose the permission mode",
            CommandAction::Picker {
                picker: PickerKind::PermissionMode,
            },
        ));
    }
    if in_thread {
        if info.capabilities.fork {
            out.push(cmd(
                "fork",
                "Fork this thread",
                CommandAction::Method {
                    method: "thread/fork".into(),
                    params: None,
                },
            ));
        }
        out.push(cmd(
            "diff",
            "Show all changes of this thread",
            CommandAction::Method {
                method: "thread/diff".into(),
                params: Some(json!({ "scope": { "kind": "thread" } })),
            },
        ));
        out.push(cmd(
            "stop",
            "Stop the agent process",
            CommandAction::Method {
                method: "thread/stop".into(),
                params: None,
            },
        ));
        out.push(cmd(
            "resume-queue",
            "Run the queued messages",
            CommandAction::Method {
                method: "queue/resume".into(),
                params: None,
            },
        ));
        out.push(cmd(
            "archive",
            "Archive this thread",
            CommandAction::Method {
                method: "thread/archive".into(),
                params: Some(json!({ "archived": true })),
            },
        ));
    }
    out
}

trait WithCap {
    fn with_cap(self, capability: &str) -> CoreError;
}

impl WithCap for CoreError {
    fn with_cap(self, capability: &str) -> CoreError {
        match self {
            CoreError::Rpc(e) => CoreError::Rpc(e.with("capability", capability)),
            other => other,
        }
    }
}

/// Brings persisted state in line after an unclean (or clean) stop of the previous run.
async fn recover(sh: &Arc<Shared>) -> CoreResult<()> {
    let preview_chars = sh.config.policy.queued_preview_chars;
    sh.tx(move |tx, em: &mut Emitter| {
        let now = now_ms();
        // The previous run stopped because Windows ended the session: the turns it could not
        // record before the end are recorded as such (`Engine::shutdown_for_end_session`).
        let session_ended = store::meta_get(tx, store::META_SESSION_ENDED_AT)?.is_some();
        store::meta_delete(tx, store::META_SESSION_ENDED_AT)?;
        let interrupted = if session_ended {
            TurnError {
                message: SYSTEM_SHUTDOWN_MESSAGE.into(),
                kind: "systemShutdown".into(),
            }
        } else {
            TurnError {
                message: "the daemon restarted during this turn".into(),
                kind: "daemonRestarted".into(),
            }
        };
        // Threads an older version only marked as removed are purged, first (nothing below
        // may emit events for them); their refs and worktrees are left to cleanup jobs.
        for row in store::removed_threads(tx)? {
            let repo = store::get_project(tx, &row.project_id)?.map(|p| p.project.path);
            store::purge_thread(tx, &row.id, now)?;
            if let Some(repo) = repo {
                store::insert_cleanup_job(tx, CleanupKind::SnapshotRefs, &repo, row.id.as_str(), now)?;
                if let Workspace::Worktree { path, .. } = &row.workspace {
                    store::insert_cleanup_job(tx, CleanupKind::Worktree, &repo, path, now)?;
                }
            }
            tracing::info!(thread = %row.id, "purged a thread removed by an earlier version");
        }
        let mut touched: HashMap<ThreadId, ()> = HashMap::new();
        for mut t in store::turns_with_status(tx, TurnStatus::Running)? {
            t.turn.status = TurnStatus::Interrupted;
            t.turn.completed_at = Some(now);
            t.turn.error = Some(interrupted.clone());
            store::update_turn(tx, &t)?;
            em.thread(&t.turn.thread_id, Event::TurnCompleted { turn: t.turn.clone() });
            touched.insert(t.turn.thread_id.clone(), ());
        }
        for mut item in store::items_in_progress(tx)? {
            item.status = ItemStatus::Interrupted;
            item.completed_at = Some(now);
            store::update_item(tx, &item)?;
            em.thread(&item.thread_id.clone(), Event::ItemCompleted { item });
        }
        for mut r in store::pending_interactions(tx)? {
            r.interaction.status = InteractionStatus::Expired;
            r.interaction.resolved_at = Some(now);
            r.interaction.resolved_by = Some("system".into());
            r.interaction.expire_reason = Some(ExpireReason::DaemonRestarted);
            store::update_interaction(tx, &r.interaction)?;
            let thread_id = r.interaction.thread_id.clone();
            em.thread(&thread_id, Event::InteractionExpired { interaction: r.interaction.clone() });
            em.workspace(Event::InteractionClosed {
                interaction_id: r.interaction.id.clone(),
                thread_id: thread_id.clone(),
                status: InteractionStatus::Expired,
            });
            touched.insert(thread_id, ());
        }
        // Background work ended with the processes of the previous run (every process is gone).
        let (task_status, task_reason) = if session_ended {
            (BackgroundTaskStatus::Stopped, BackgroundEndReason::SystemShutdown)
        } else {
            (BackgroundTaskStatus::Lost, BackgroundEndReason::DaemonRestarted)
        };
        for mut task in store::running_background_tasks(tx)? {
            task.status = task_status;
            task.ended_at = Some(now);
            task.end_reason = Some(task_reason);
            task.stop_requested_at = None;
            store::upsert_background_task(tx, &task)?;
            touched.insert(task.thread_id.clone(), ());
            em.thread(&task.thread_id.clone(), Event::BackgroundTaskUpdated { task });
        }
        let busy = [ThreadStatus::Queued, ThreadStatus::Starting, ThreadStatus::Ready, ThreadStatus::Running, ThreadStatus::Stopping];
        for row in store::threads_with_status(tx, &busy)? {
            touched.insert(row.id.clone(), ());
        }
        for id in touched.keys() {
            if let Some(mut row) = store::get_thread(tx, id)? {
                row.status = ThreadStatus::Idle;
                if !store::list_queued(tx, id, preview_chars)?.is_empty() {
                    row.queue_paused = true;
                }
                thread_changed(tx, em, &mut row)?;
            }
        }
        for (mut op, work_dir) in store::unfinished_operations(tx)? {
            // The folder a clone was writing into is incomplete: remove it (a failure is
            // logged and retried at the next start).
            let mut removed = true;
            if let Some(dir) = &work_dir
                && let Err(e) = operations::remove_dir_all(Path::new(dir))
            {
                tracing::warn!(operation = %op.id, folder = %dir, error = %e, "could not remove the folder of an interrupted operation");
                removed = false;
            }
            if removed {
                store::clear_operation_work_dir(tx, &op.id)?;
            }
            if op.status == OperationStatus::Running {
                op.status = OperationStatus::Failed;
                op.finished_at = Some(now);
                op.message = Some(operations::DAEMON_STOPPED.into());
                op.progress = None;
                store::upsert_operation(tx, &op)?;
                em.workspace(Event::OperationUpdated { operation: op });
            }
        }
        if !touched.is_empty() {
            tracing::info!(threads = touched.len(), "recovered threads after restart");
        }
        Ok(())
    })
    .await
}
