//! One task per thread: owns the agent session, the running turn, its items and
//! interactions, the input queue and the process lifecycle (design.md §5).
//!
//! All state changes of a thread go through its actor, so they are applied in arrival order.
//! Adapter events are applied in batches: whatever is available when the actor wakes up is
//! written in one transaction (no timers involved).
//!
//! The actor also tracks the background tasks of its agent process (design.md §5.6): they keep
//! the process alive while the harness reports them busy, and they end with the process.

use std::collections::HashMap;
use std::future::Future;
use std::path::{Component, Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use aas_harness::{
    AdapterError, AdapterEvent, BackgroundTaskInfo, SessionControl, SessionHandle, SettingsApplied,
    StartMode, StartRequest, TurnInput, TurnInputPart,
};
use aas_protocol::events::Event;
use aas_protocol::methods::*;
use aas_protocol::*;
use aas_supervisor::{ExitInfo, PowerLease, StopReason};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;

use crate::background::{self, Background, Tracked};
use crate::blobs::{BlobPin, Spill};
use crate::capacity::Permit;
use crate::emit::{Emitter, thread_changed};
use crate::error::{CoreError, CoreResult, invalid_params, invalid_state, not_found, rpc};
use crate::registry::unavailable_error;
use crate::shared::{Idem, Publish, Shared};
use crate::store::{self, InteractionRow, ThreadRow, TurnRow, now_ms};

pub type Reply<T> = oneshot::Sender<Result<T, RpcError>>;

/// Requests handled by a thread actor.
pub enum Msg {
    StartTurn {
        input: Vec<InputPart>,
        delivery: Delivery,
        idem: Option<Idem>,
        /// Also insert the (new) thread row in the same transaction; the reply becomes the
        /// `thread/create` result.
        create: bool,
        reply: Reply<TurnStartResult>,
    },
    ResumeQueue {
        idem: Option<Idem>,
        reply: Reply<QueueResumeResult>,
    },
    RemoveQueued {
        queued_id: QueuedInputId,
        idem: Option<Idem>,
        reply: Reply<QueueRemoveResult>,
    },
    Interrupt {
        idem: Option<Idem>,
        reply: Reply<TurnInterruptResult>,
    },
    Respond {
        interaction_id: InteractionId,
        resolution: InteractionResolution,
        device: DeviceId,
        idem: Option<Idem>,
        reply: Reply<InteractionRespondResult>,
    },
    Update {
        title: Option<String>,
        settings: Option<ThreadSettings>,
        pinned: Option<bool>,
        idem: Option<Idem>,
        reply: Reply<ThreadUpdateResult>,
    },
    /// Replaces the input of a queued entry (keeps its place in the queue).
    UpdateQueued {
        queued_id: QueuedInputId,
        input: Vec<InputPart>,
        idem: Option<Idem>,
        reply: Reply<QueueUpdateResult>,
    },
    /// Sends a queued entry now: into the running turn (steer), or as a new turn.
    SteerQueued {
        queued_id: QueuedInputId,
        idem: Option<Idem>,
        reply: Reply<QueueSteerResult>,
    },
    Stop {
        idem: Option<Idem>,
        reply: Reply<ThreadResult>,
    },
    Archive {
        archived: bool,
        remove_worktree: bool,
        force: bool,
        idem: Option<Idem>,
        reply: Reply<ThreadResult>,
    },
    /// The thread is about to be removed: fails with `invalidState` while anything runs;
    /// otherwise the actor writes nothing more and holds every later request until the
    /// removal is decided. When it succeeds the engine drops the actor's handles and the held
    /// requests are answered with `notFound`; when it fails, [`Msg::Unretire`] follows and
    /// they are handled in their order.
    Retire {
        reply: oneshot::Sender<CoreResult<()>>,
    },
    /// The removal the actor was retired for failed: the thread carries on as before.
    Unretire,
    Commands {
        reply: oneshot::Sender<Option<Vec<Command>>>,
    },
    /// `backgroundTask/stop`: asks the harness to stop one background task.
    StopBackground {
        task_id: BackgroundTaskId,
        idem: Option<Idem>,
        reply: Reply<BackgroundTaskResult>,
    },
    /// Daemon shutdown: stop the process (if any) and exit the actor.
    Shutdown { reply: oneshot::Sender<()> },
}

#[derive(Clone)]
pub struct ActorHandle {
    tx: mpsc::UnboundedSender<Msg>,
    /// The thread's harness (it never changes for a thread).
    harness_id: Arc<str>,
}

/// The actor has exited (its message was dropped).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActorGone;

impl ActorHandle {
    pub fn send(&self, msg: Msg) -> Result<(), ActorGone> {
        self.tx.send(msg).map_err(|_| ActorGone)
    }

    pub fn is_closed(&self) -> bool {
        self.tx.is_closed()
    }

    pub fn harness_id(&self) -> &str {
        &self.harness_id
    }
}

/// Spawns the actor of `row`. `persisted = false` for a thread that is inserted by its first
/// `StartTurn { create: true }`. `last_turn` is the thread's latest turn.
pub fn spawn(
    sh: Arc<Shared>,
    row: ThreadRow,
    persisted: bool,
    queue_len: usize,
    next_turn_index: u32,
    last_turn: Option<TurnId>,
) -> ActorHandle {
    let (tx, rx) = mpsc::unbounded_channel();
    let harness_id: Arc<str> = Arc::from(row.harness_id.as_str());
    let actor = Actor {
        sh,
        row,
        persisted,
        rx,
        rx_closed: false,
        next_turn_index,
        live: None,
        turn: None,
        deferred: None,
        launch: None,
        idle_deadline: None,
        interrupt_deadline: None,
        stopping: None,
        restart_pending: false,
        commands: None,
        reported_model: None,
        queue_len,
        shutting_down: false,
        retired: false,
        held: std::collections::VecDeque::new(),
        exit_waiters: Vec::new(),
        background: Background::default(),
        pending: HashMap::new(),
        last_turn,
        background_lease: None,
        background_busy: 0,
    };
    tokio::spawn(actor.run());
    ActorHandle { tx, harness_id }
}

struct Live {
    control: Arc<dyn SessionControl>,
    events: mpsc::UnboundedReceiver<AdapterEvent>,
    permit: Permit,
    /// Settings the process runs with: those it was started with, then every change it
    /// applied live. They differ from the thread's settings while a change waits for the next
    /// turn (a running turn is never affected, design.md §5.4).
    settings: ThreadSettings,
}

struct OpenItem {
    /// The adapter's key of the item.
    key: String,
    item: Item,
    spill: Option<Spill>,
    truncated: bool,
}

/// What a pending interaction belongs to: it expires with it (design.md §8).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Scope {
    Turn(TurnId),
    Task(BackgroundTaskId),
    /// The agent asked while no turn ran and named no background task.
    Thread,
}

/// An interaction that waits for an answer, by the adapter's request id.
struct PendingInteraction {
    id: InteractionId,
    scope: Scope,
}

struct ActiveTurn {
    id: TurnId,
    index: u32,
    started_at: Millis,
    /// Input to send once the session is ready.
    input: TurnInput,
    sent: bool,
    items: HashMap<String, OpenItem>,
    order: Vec<String>,
    /// Item ids of every item of the turn by the adapter's key, closed ones included (a
    /// background task names the item that launched it by its key).
    keys: HashMap<String, ItemId>,
    usage: Option<Usage>,
    base_tree: Option<String>,
    _lease: Option<PowerLease>,
    interrupt_requested: bool,
    forced: bool,
    /// Why the harness started this run by itself (reported with its completion).
    trigger: Option<TurnTrigger>,
    /// The notice that the turn waits for background work has been added.
    background_notice: bool,
}

enum Phase {
    Capacity(Pin<Box<dyn Future<Output = Permit> + Send>>),
    Launching(Pin<Box<dyn Future<Output = LaunchResult> + Send>>),
}

/// What a launch does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LaunchKind {
    /// The process is alive: only the turn's base snapshot is taken.
    Reuse,
    /// No process: a new one is started.
    Fresh,
    /// The process is replaced (settings that need a restart); its permit is reused.
    Restart,
}

struct Launch {
    phase: Phase,
    /// The turn this launch was started for.
    turn: TurnId,
    kind: LaunchKind,
    /// Settings a new process is started with (compared with the thread's settings once it
    /// runs: they may have changed meanwhile).
    settings: ThreadSettings,
    /// Set when no turn needs the process any more: the start is skipped if it has not begun
    /// yet. A start in progress is never dropped (see `Actor::cancel_launch`).
    abort: Arc<AtomicBool>,
}

struct LaunchResult {
    base_tree: Option<String>,
    session: Option<Result<(SessionHandle, Permit), AdapterError>>,
    /// The start was refused before any process was started: the fork's parent has moved on
    /// (see [`fork_point_passed`]).
    refused: Option<TurnError>,
}

/// Why a launch produced no process for its turn.
enum StartFailure {
    Adapter(AdapterError),
    Refused(TurnError),
}

impl StartFailure {
    /// The error the turn ends with.
    fn turn_error(&self) -> TurnError {
        match self {
            StartFailure::Adapter(e) => {
                let kind = match e {
                    AdapterError::Unavailable(_) => "harnessUnavailable",
                    _ => "spawnFailed",
                };
                TurnError {
                    message: e.to_string(),
                    kind: kind.into(),
                }
            }
            StartFailure::Refused(error) => error.clone(),
        }
    }
}

impl std::fmt::Display for StartFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StartFailure::Adapter(e) => write!(f, "{e}"),
            StartFailure::Refused(error) => write!(f, "{} ({})", error.message, error.kind),
        }
    }
}

enum LaunchStep {
    Permit(Permit),
    Done(LaunchResult),
}

struct ArchiveReq {
    archived: bool,
    remove_worktree: bool,
    force: bool,
}

struct Stopping {
    reason: StopReason,
    waiters: Vec<(Option<Idem>, Reply<ThreadResult>)>,
    archive: Vec<(ArchiveReq, Option<Idem>, Reply<ThreadResult>)>,
}

/// Changes of one transaction.
#[derive(Default)]
struct Uow {
    thread_insert: bool,
    turn_inserts: Vec<TurnRow>,
    turn_updates: Vec<TurnRow>,
    items_insert: Vec<Item>,
    items_update: Vec<Item>,
    interactions_insert: Vec<InteractionRow>,
    interactions_update: Vec<Interaction>,
    expire: Vec<(InteractionId, ExpireReason)>,
    queued_insert: Vec<QueuedInput>,
    queued_update: Vec<(QueuedInputId, Vec<InputPart>)>,
    queued_delete: Vec<QueuedInputId>,
    /// Background tasks to store (whole state; a later entry of the same task wins).
    background: Vec<BackgroundTask>,
    blobs: Vec<(BlobId, String, u64)>,
    events: Vec<Event>,
    ws_events: Vec<Event>,
    queue_changed: bool,
    thread_changed: bool,
    idem_values: Vec<(Idem, Value)>,
    idem_thread: Vec<Idem>,
    /// `thread/create` with input: the stored result embeds the thread view of this commit.
    idem_create: Option<(Idem, TurnStartResult)>,
    /// Blobs this commit refers to, kept from deletion until it has committed. Taken out
    /// before the changes are handed to the database (a pin must never drop inside it).
    pins: Vec<BlobPin>,
    /// Requests of the agent that expire in this commit while its process lives: the adapter
    /// answers each once the commit is stored (design.md §8).
    answers: Vec<(String, ExpireReason)>,
}

impl Uow {
    fn is_empty(&self) -> bool {
        !self.thread_insert
            && self.turn_inserts.is_empty()
            && self.turn_updates.is_empty()
            && self.items_insert.is_empty()
            && self.items_update.is_empty()
            && self.interactions_insert.is_empty()
            && self.interactions_update.is_empty()
            && self.expire.is_empty()
            && self.queued_insert.is_empty()
            && self.queued_update.is_empty()
            && self.queued_delete.is_empty()
            && self.background.is_empty()
            && self.blobs.is_empty()
            && self.events.is_empty()
            && self.ws_events.is_empty()
            && !self.queue_changed
            && !self.thread_changed
            && self.idem_values.is_empty()
            && self.idem_thread.is_empty()
            && self.idem_create.is_none()
            && self.answers.is_empty()
    }

    fn update_item(&mut self, item: Item) {
        if let Some(pos) = self.items_insert.iter().position(|i| i.id == item.id) {
            self.items_insert[pos] = item;
        } else if let Some(pos) = self.items_update.iter().position(|i| i.id == item.id) {
            self.items_update[pos] = item;
        } else {
            self.items_update.push(item);
        }
    }
}

/// Work to do after a transaction commits.
enum After {
    /// Summarize `base..head` of a turn whose end snapshot `head` is stored with it.
    DiffSummary {
        turn: TurnId,
        base: String,
        head: String,
    },
    StartNextQueued,
    /// Start the user's turn that waited behind an agent-initiated run.
    ResumeDeferred,
    Reprobe,
}

struct Actor {
    sh: Arc<Shared>,
    row: ThreadRow,
    persisted: bool,
    rx: mpsc::UnboundedReceiver<Msg>,
    rx_closed: bool,
    next_turn_index: u32,
    live: Option<Live>,
    turn: Option<ActiveTurn>,
    /// A user turn whose input had not been sent when the agent started a run by itself: it
    /// waits until that run (the current `turn`) has ended.
    deferred: Option<ActiveTurn>,
    launch: Option<Launch>,
    idle_deadline: Option<Instant>,
    interrupt_deadline: Option<Instant>,
    stopping: Option<Stopping>,
    restart_pending: bool,
    commands: Option<Vec<Command>>,
    reported_model: Option<String>,
    queue_len: usize,
    /// A shutdown was requested (by the engine, or because every handle was dropped): the
    /// actor exits as soon as no process and no launch is left.
    shutting_down: bool,
    /// The thread is being removed ([`Msg::Retire`]): nothing is written any more and requests
    /// wait in `held` until the removal is decided.
    retired: bool,
    /// Requests that arrived while retired, in arrival order.
    held: std::collections::VecDeque<Msg>,
    /// Callers of [`Msg::Shutdown`] waiting for that exit. Kept apart from `shutting_down` so
    /// the mailbox closing right after a `Shutdown` (the engine drops its handles) cannot
    /// discard a waiter.
    exit_waiters: Vec<oneshot::Sender<()>>,
    /// Background tasks of the current process.
    background: Background,
    /// Interactions waiting for an answer, by the adapter's request id.
    pending: HashMap<String, PendingInteraction>,
    /// The thread's latest turn (running or not): what a background task or an interaction
    /// that belongs to no turn is shown with.
    last_turn: Option<TurnId>,
    /// Held while background work keeps the agent busy (`policy.prevent_sleep_while_running`).
    background_lease: Option<PowerLease>,
    /// The busy background tasks this actor counts in `Shared::running_background`.
    background_busy: usize,
}

/// Input validated and converted for the adapter, with pins on the blobs it refers to.
pub(crate) struct ConvertedInput {
    input: TurnInput,
    attachments: Vec<Attachment>,
    mentions: Vec<Mention>,
    text: String,
    pins: Vec<BlobPin>,
}

async fn recv_live(live: &mut Option<Live>) -> Option<AdapterEvent> {
    match live {
        Some(l) => l.events.recv().await,
        None => std::future::pending().await,
    }
}

async fn poll_launch(launch: &mut Option<Launch>) -> LaunchStep {
    match launch {
        Some(Launch {
            phase: Phase::Capacity(f),
            ..
        }) => LaunchStep::Permit(f.await),
        Some(Launch {
            phase: Phase::Launching(f),
            ..
        }) => LaunchStep::Done(f.await),
        None => std::future::pending().await,
    }
}

async fn sleep_opt(deadline: Option<Instant>) {
    match deadline {
        Some(d) => tokio::time::sleep_until(d).await,
        None => std::future::pending().await,
    }
}

/// Bounds a request to the agent made from the actor's loop. While the actor waits for it,
/// nothing else of the thread is handled — adapter events, `thread/stop`, the daemon's
/// shutdown — so a CLI that stops answering must not keep it waiting for good. Adapters bound
/// their own requests too; this is the engine's guarantee for every adapter
/// (`policy.handshake_timeout`, or the interrupt's deadline).
async fn bounded<T>(
    deadline: Instant,
    what: &'static str,
    call: impl Future<Output = Result<T, AdapterError>>,
) -> Result<T, AdapterError> {
    let started = Instant::now();
    match tokio::time::timeout_at(deadline, call).await {
        Ok(result) => result,
        Err(_) => Err(AdapterError::Protocol(format!(
            "the agent did not answer {what} within {:?}",
            deadline.saturating_duration_since(started)
        ))),
    }
}

fn send_reply<T>(reply: Reply<T>, result: CoreResult<T>) {
    let _ = reply.send(result.map_err(RpcError::from));
}

impl Actor {
    async fn run(mut self) {
        loop {
            // Biased: what is already queued goes first — requests, then the agent's events —
            // before any timer. An event the agent sent before the idle deadline passed (a run
            // it starts by itself, background work it reports) is applied before the idle stop
            // is considered, and the stop re-checks what it may stop.
            tokio::select! {
                biased;
                msg = self.rx.recv(), if !self.rx_closed => match msg {
                    Some(msg) => {
                        self.on_msg(msg).await;
                        // A failed removal released the requests held meanwhile.
                        while !self.retired
                            && let Some(held) = self.held.pop_front()
                        {
                            self.on_msg(held).await;
                        }
                    }
                    None => {
                        // Engine dropped every handle: behave like a shutdown (this also
                        // happens right after a `Shutdown`, whose waiter stays registered).
                        // A retired thread has been removed: what it held is answered as such.
                        self.rx_closed = true;
                        while let Some(held) = self.held.pop_front() {
                            self.refuse(held);
                        }
                        self.on_shutdown(None).await;
                    }
                },
                ev = recv_live(&mut self.live) => self.on_events(ev).await,
                step = poll_launch(&mut self.launch) => self.on_launch_step(step).await,
                _ = sleep_opt(self.interrupt_deadline) => self.on_interrupt_deadline().await,
                _ = sleep_opt(self.background.next_deadline()) => self.on_background_deadline().await,
                _ = sleep_opt(self.idle_deadline) => self.on_idle_deadline().await,
            }
            if self.shutting_down && self.live.is_none() && self.launch.is_none() {
                // Without a process nothing keeps the agent busy (every task ended with it).
                self.background.clear();
                self.refresh_background_hold();
                for waiter in self.exit_waiters.drain(..) {
                    let _ = waiter.send(());
                }
                break;
            }
        }
    }

    // ----- status ------------------------------------------------------------------------------

    fn desired_status(&self) -> ThreadStatus {
        if self.stopping.is_some() && (self.live.is_some() || self.starting_process()) {
            ThreadStatus::Stopping
        } else if let Some(l) = &self.launch {
            match l.phase {
                Phase::Capacity(_) => ThreadStatus::Queued,
                Phase::Launching(_) => ThreadStatus::Starting,
            }
        } else if self.turn.is_some() {
            ThreadStatus::Running
        } else if self.live.is_some() {
            ThreadStatus::Ready
        } else {
            ThreadStatus::Idle
        }
    }

    /// Whether a process may be idle-reaped: nothing runs, nothing will start by itself (a
    /// paused queue waits for the user, who may take any time), and no background task keeps
    /// the agent busy (the harness's live set holds nothing but ambient work). Background work
    /// is never stopped because of time (design.md §4.7).
    fn reapable(&self) -> bool {
        self.desired_status() == ThreadStatus::Ready
            && (self.queue_len == 0 || self.row.queue_paused)
            && self.stopping.is_none()
            && self.background.busy() == 0
    }

    /// Keeps the daemon's count of busy background tasks and this thread's sleep lease in line
    /// with the tasks of the process.
    fn refresh_background_hold(&mut self) {
        let busy = self.background.busy();
        self.sh.background_busy_changed(self.background_busy, busy);
        self.background_busy = busy;
        let hold = busy > 0 && self.sh.config.policy.prevent_sleep_while_running;
        if hold && self.background_lease.is_none() {
            self.background_lease = Some(self.sh.supervisor.power().acquire());
        } else if !hold {
            self.background_lease = None;
        }
    }

    fn refresh_status(&mut self, uow: &mut Uow) {
        let desired = self.desired_status();
        if self.row.status != desired {
            self.row.status = desired;
            uow.thread_changed = true;
        }
        // Idle reaping: armed when the process becomes idle (from then on), disarmed as soon as
        // it is not (a turn, a queued input, busy background work).
        if self.reapable() {
            if self.idle_deadline.is_none() {
                self.idle_deadline = Some(Instant::now() + self.sh.config.policy.idle_process_ttl);
            }
        } else {
            self.idle_deadline = None;
        }
        self.refresh_background_hold();
    }

    /// Persists `uow` (with the thread's current row) in one transaction. A storage failure
    /// is retried; when it persists, the daemon fail-stops (`Shared::tx_durable`): the changes
    /// of a thread are never dropped while the daemon keeps running.
    async fn commit(&mut self, mut uow: Uow) -> CoreResult<Option<Thread>> {
        if self.retired {
            return Err(not_found("thread", &self.row.id));
        }
        self.refresh_status(&mut uow);
        // Released only after the commit (see `Uow::pins`).
        let pins = std::mem::take(&mut uow.pins);
        let answers = std::mem::take(&mut uow.answers);
        if uow.is_empty() {
            return Ok(None);
        }
        if uow.thread_changed || uow.thread_insert {
            self.row.updated_at = now_ms();
        }
        let row = self.row.clone();
        let uow = Arc::new(uow);
        let changes = uow.clone();
        let preview_chars = self.sh.config.policy.queued_preview_chars;
        let result = self
            .sh
            .tx_durable("thread changes", move |tx, em| {
                apply_uow(tx, em, &changes, row.clone(), preview_chars)
            })
            .await;
        drop(uow);
        drop(pins);
        let (view, head) = result?;
        self.row.head = head;
        self.persisted = true;
        self.answer_expired(answers).await;
        Ok(view)
    }

    /// Answers the agent's requests that expired while its process lives (their turn or
    /// background task ended), so that the CLI does not wait for them (design.md §8). A request
    /// the adapter no longer knows was answered or withdrawn meanwhile.
    async fn answer_expired(&mut self, answers: Vec<(String, ExpireReason)>) {
        let Some(control) = self.live.as_ref().map(|l| l.control.clone()) else {
            return;
        };
        for (request_id, reason) in answers {
            let deadline = self.request_deadline();
            match bounded(
                deadline,
                "an expired request",
                control.expire_request(&request_id, reason),
            )
            .await
            {
                Ok(()) => {
                    tracing::debug!(thread = %self.row.id, request_id, ?reason, "answered an expired request")
                }
                Err(AdapterError::UnknownRequest(_) | AdapterError::Closed) => {
                    tracing::debug!(thread = %self.row.id, request_id, "the expired request is no longer pending")
                }
                Err(e) => {
                    tracing::warn!(thread = %self.row.id, request_id, error = %e, "answering an expired request failed")
                }
            }
        }
    }

    /// Commits where no reply is waiting. A failure has already taken the fail-stop path
    /// (see [`commit`](Self::commit)); it is logged here.
    async fn commit_logged(&mut self, uow: Uow) {
        if let Err(e) = self.commit(uow).await {
            if self.sh.failed() {
                tracing::warn!(thread = %self.row.id, error = %e, "thread changes not persisted during the fail-stop");
            } else {
                tracing::error!(thread = %self.row.id, error = %e, "failed to persist thread changes");
            }
        }
    }

    fn current_view(&self) -> impl Future<Output = CoreResult<Thread>> + Send + 'static {
        let row = self.row.clone();
        let db = self.sh.db.clone();
        async move { db.read(move |tx| store::thread_view(tx, &row)).await }
    }

    // ----- messages ----------------------------------------------------------------------------

    async fn on_msg(&mut self, msg: Msg) {
        if self.retired {
            match msg {
                Msg::Retire { reply } => {
                    let _ = reply.send(Ok(()));
                }
                Msg::Unretire => {
                    // The run loop handles what was held.
                    self.retired = false;
                }
                Msg::Commands { reply } => {
                    let _ = reply.send(None);
                }
                Msg::Shutdown { reply } => {
                    // Nothing runs and nothing may be written.
                    self.shutting_down = true;
                    self.exit_waiters.push(reply);
                }
                other => self.held.push_back(other),
            }
            return;
        }
        match msg {
            Msg::StartTurn {
                input,
                delivery,
                idem,
                create,
                reply,
            } => {
                let r = self.start_turn_request(input, delivery, idem, create).await;
                send_reply(reply, r);
            }
            Msg::ResumeQueue { idem, reply } => {
                let r = self.resume_queue(idem).await;
                send_reply(reply, r);
            }
            Msg::RemoveQueued {
                queued_id,
                idem,
                reply,
            } => {
                let r = self.remove_queued(queued_id, idem).await;
                send_reply(reply, r);
            }
            Msg::Interrupt { idem, reply } => {
                let r = self.interrupt(idem).await;
                send_reply(reply, r);
            }
            Msg::Respond {
                interaction_id,
                resolution,
                device,
                idem,
                reply,
            } => {
                let r = self.respond(interaction_id, resolution, device, idem).await;
                send_reply(reply, r);
            }
            Msg::Update {
                title,
                settings,
                pinned,
                idem,
                reply,
            } => {
                let r = self.update(title, settings, pinned, idem).await;
                send_reply(reply, r);
            }
            Msg::UpdateQueued {
                queued_id,
                input,
                idem,
                reply,
            } => {
                let r = self.update_queued(queued_id, input, idem).await;
                send_reply(reply, r);
            }
            Msg::SteerQueued {
                queued_id,
                idem,
                reply,
            } => {
                let r = self.steer_queued(queued_id, idem).await;
                send_reply(reply, r);
            }
            Msg::Stop { idem, reply } => self.stop(idem, reply).await,
            Msg::Archive {
                archived,
                remove_worktree,
                force,
                idem,
                reply,
            } => {
                self.archive(
                    ArchiveReq {
                        archived,
                        remove_worktree,
                        force,
                    },
                    idem,
                    reply,
                )
                .await
            }
            Msg::Retire { reply } => {
                let _ = reply.send(self.retire());
            }
            Msg::Unretire => {}
            Msg::Commands { reply } => {
                let _ = reply.send(self.commands.clone());
            }
            Msg::StopBackground {
                task_id,
                idem,
                reply,
            } => {
                let r = self.stop_background(task_id, idem).await;
                send_reply(reply, r);
            }
            Msg::Shutdown { reply } => self.on_shutdown(Some(reply)).await,
        }
    }

    /// The thread's harness information, or `harnessUnavailable` (with `data.harnessId` and
    /// `data.reason`). The engine has probed an unavailable harness again before the request
    /// reached the actor ([`crate::shared::Shared::recheck_unavailable_harness`]).
    fn harness_info(&self) -> CoreResult<aas_harness::HarnessInfo> {
        let harness = &self.row.harness_id;
        if self.sh.registry.get(harness).is_none() {
            return Err(rpc(
                ErrorKind::HarnessUnavailable,
                format!("harness {harness} is not configured"),
            )
            .with("harnessId", harness.as_str())
            .with("reason", "not configured in config.toml"));
        }
        match self.sh.registry.info(harness) {
            Some(info) if info.available => Ok(info),
            other => Err(unavailable_error(harness, other.as_ref())),
        }
    }

    /// Answers a request held by a thread that has been removed, as if it had never existed.
    fn refuse(&mut self, msg: Msg) {
        let gone = || RpcError::from(not_found("thread", &self.row.id));
        match msg {
            Msg::StartTurn { reply, .. } => {
                let _ = reply.send(Err(gone()));
            }
            Msg::ResumeQueue { reply, .. } => {
                let _ = reply.send(Err(gone()));
            }
            Msg::RemoveQueued { reply, .. } => {
                let _ = reply.send(Err(gone()));
            }
            Msg::Interrupt { reply, .. } => {
                let _ = reply.send(Err(gone()));
            }
            Msg::Respond { reply, .. } => {
                let _ = reply.send(Err(gone()));
            }
            Msg::Update { reply, .. } => {
                let _ = reply.send(Err(gone()));
            }
            Msg::UpdateQueued { reply, .. } => {
                let _ = reply.send(Err(gone()));
            }
            Msg::SteerQueued { reply, .. } => {
                let _ = reply.send(Err(gone()));
            }
            Msg::StopBackground { reply, .. } => {
                let _ = reply.send(Err(gone()));
            }
            Msg::Stop { reply, .. } | Msg::Archive { reply, .. } => {
                let _ = reply.send(Err(gone()));
            }
            Msg::Retire { reply } => {
                let _ = reply.send(Ok(()));
            }
            Msg::Unretire => {}
            Msg::Commands { reply } => {
                let _ = reply.send(None);
            }
            Msg::Shutdown { reply } => {
                self.shutting_down = true;
                self.exit_waiters.push(reply);
            }
        }
    }

    /// See [`Msg::Retire`].
    fn retire(&mut self) -> CoreResult<()> {
        if self.live.is_some()
            || self.turn.is_some()
            || self.deferred.is_some()
            || self.launch.is_some()
            || self.stopping.is_some()
        {
            return Err(invalid_state("the thread is running; stop it first"));
        }
        self.retired = true;
        self.idle_deadline = None;
        self.interrupt_deadline = None;
        Ok(())
    }

    /// Validates input parts and converts them for the adapter.
    fn convert_input(
        &self,
        input: &[InputPart],
    ) -> impl Future<Output = CoreResult<ConvertedInput>> + Send + 'static {
        let sh = self.sh.clone();
        let cwd = PathBuf::from(&self.row.cwd);
        let input = input.to_vec();
        async move { convert_input(sh, cwd, input).await }
    }

    /// [`convert_input`](Self::convert_input) plus the checks against the harness a new
    /// turn's input needs (see [`check_input`]).
    fn check_input(
        &self,
        input: &[InputPart],
        info: &aas_harness::HarnessInfo,
    ) -> impl Future<Output = CoreResult<ConvertedInput>> + Send + 'static {
        let (sh, cwd, input, images) = (
            self.sh.clone(),
            PathBuf::from(&self.row.cwd),
            input.to_vec(),
            info.capabilities.images,
        );
        async move { check_input(sh, cwd, input, images).await }
    }

    /// The deadline of a request to the agent that starts now (see [`bounded`]).
    fn request_deadline(&self) -> Instant {
        Instant::now() + self.sh.config.policy.handshake_timeout
    }

    /// Fails with `invalidState` when this thread is a fork whose native session is still to
    /// be branched off and its parent has run turns since the fork was made (see
    /// [`fork_point_passed`]).
    fn check_fork_point(&self) -> impl Future<Output = CoreResult<()>> + Send + 'static {
        let pending = self
            .row
            .fork_source
            .is_some()
            .then(|| self.row.forked_from.clone())
            .flatten();
        let db = self.sh.db.clone();
        async move {
            let Some(origin) = pending else { return Ok(()) };
            if fork_point_passed(&db, &origin).await? {
                return Err(invalid_state(FORK_OUTDATED_MESSAGE));
            }
            Ok(())
        }
    }
}

/// Why the first turn of an outdated fork is refused (`invalidState`, or the turn's error
/// `forkOutdated` when the parent moved on while the turn waited for its process).
const FORK_OUTDATED_MESSAGE: &str = "the thread this fork was made from has run further turns since; its agent session can no longer be branched at the fork's point (fork it again)";

/// Whether the parent of a fork has run turns since the fork was made.
///
/// A fork's native session is branched off the parent's (`StartMode::Fork`) when the fork's
/// first process starts, and every harness copies the parent's session as it is at that
/// moment. The fork shows the parent's history up to `origin.turn_id` (the parent's last turn
/// when the fork was made); once the parent has a later turn (running or finished), the
/// branched session would contain work the fork's history does not show. A parent that has
/// been removed runs nothing any more.
async fn fork_point_passed(db: &crate::db::Db, origin: &ForkOrigin) -> CoreResult<bool> {
    let parent = origin.thread_id.clone();
    let last = db
        .read(move |tx| {
            let Some(row) = store::get_thread(tx, &parent)?.filter(|r| !r.removed) else {
                return Ok(None);
            };
            let (turns, _) = store::list_turns(tx, &row.id, None, 1)?;
            Ok(Some(turns.into_iter().next().map(|t| t.turn.id)))
        })
        .await?;
    Ok(match last {
        None => false,
        Some(last) => last != origin.turn_id,
    })
}

/// Validates the input of a turn as `turn/start` does (see [`convert_input`]) and checks it
/// against the harness: images need the capability `images`. `cwd` is the folder mentions are
/// relative to.
pub(crate) async fn check_input(
    sh: Arc<Shared>,
    cwd: PathBuf,
    input: Vec<InputPart>,
    images: bool,
) -> CoreResult<ConvertedInput> {
    let converted = convert_input(sh, cwd, input).await?;
    if !images && !converted.attachments.is_empty() {
        return Err(rpc(
            ErrorKind::CapabilityUnsupported,
            "this harness does not accept images",
        )
        .with("capability", "images"));
    }
    Ok(converted)
}

async fn convert_input(
    sh: Arc<Shared>,
    cwd: PathBuf,
    input: Vec<InputPart>,
) -> CoreResult<ConvertedInput> {
    let input = &input;
    if input.is_empty() {
        return Err(invalid_params("input must not be empty"));
    }
    let mut parts = Vec::new();
    let mut attachments = Vec::new();
    let mut mentions = Vec::new();
    let mut text = String::new();
    let mut has_content = false;
    let mut pins = Vec::new();
    for part in input {
        match part {
            InputPart::Text { text: t } => {
                has_content |= !t.trim().is_empty();
                text.push_str(t);
                parts.push(TurnInputPart::Text(t.clone()));
            }
            InputPart::Mention { path } => {
                let relative = path.replace('\\', "/");
                let rel_path = Path::new(&relative);
                if rel_path.is_absolute()
                    || rel_path.components().any(|c| {
                        matches!(
                            c,
                            Component::ParentDir | Component::Prefix(_) | Component::RootDir
                        )
                    })
                {
                    return Err(invalid_params(format!(
                        "mention {path:?} must be relative to the thread's folder"
                    )));
                }
                has_content = true;
                if !text.is_empty() && !text.ends_with(char::is_whitespace) {
                    text.push(' ');
                }
                text.push('@');
                text.push_str(&relative);
                mentions.push(Mention {
                    path: relative.clone(),
                });
                parts.push(TurnInputPart::Mention {
                    absolute: cwd.join(&relative),
                    relative,
                });
            }
            InputPart::Image { blob_id } => {
                // Pinned before it is looked up: once found, it cannot be deleted before
                // the reference to it is committed.
                pins.push(sh.blobs.pin(blob_id));
                let id = blob_id.clone();
                let meta = sh.db.read(move |tx| store::get_blob(tx, &id)).await?;
                let Some((mime, _)) = meta else {
                    return Err(not_found("blob", blob_id));
                };
                let path = sh
                    .blobs
                    .path_of(blob_id)
                    .ok_or_else(|| invalid_params("malformed blob id"))?;
                has_content = true;
                attachments.push(Attachment::Image {
                    blob_id: blob_id.clone(),
                    mime: mime.clone(),
                });
                parts.push(TurnInputPart::Image { path, mime });
            }
        }
    }
    if !has_content {
        return Err(invalid_params("input must not be empty"));
    }
    Ok(ConvertedInput {
        input: TurnInput { parts },
        attachments,
        mentions,
        text,
        pins,
    })
}

/// Writes the changes of one commit of a thread actor (called again for every attempt, so
/// it only reads `uow`). `preview_chars` is `policy.queued_preview_chars`.
fn apply_uow(
    tx: &rusqlite::Transaction<'_>,
    em: &mut Emitter,
    uow: &Uow,
    mut row: ThreadRow,
    preview_chars: usize,
) -> CoreResult<(Option<Thread>, u64)> {
    let thread_id = row.id.clone();
    let now = now_ms();
    if uow.thread_insert {
        store::insert_thread(tx, &row)?;
    }
    for t in &uow.turn_inserts {
        store::insert_turn(tx, t)?;
    }
    // Blob records before the items that refer to them (an item's reference ends the grace
    // period a new record starts).
    for (id, mime, size) in &uow.blobs {
        store::insert_blob(tx, id, mime, *size, now)?;
    }
    for i in &uow.items_insert {
        store::insert_item(tx, i)?;
    }
    for i in &uow.items_update {
        store::update_item(tx, i)?;
    }
    for r in &uow.interactions_insert {
        store::insert_interaction(tx, r)?;
    }
    for i in &uow.interactions_update {
        store::update_interaction(tx, i)?;
    }
    for q in &uow.queued_insert {
        store::insert_queued(tx, q)?;
    }
    for (id, input) in &uow.queued_update {
        store::update_queued(tx, id, input)?;
    }
    for q in &uow.queued_delete {
        store::delete_queued(tx, q)?;
    }
    for t in &uow.turn_updates {
        store::update_turn(tx, t)?;
    }
    for task in &uow.background {
        store::upsert_background_task(tx, task)?;
    }
    for e in &uow.events {
        em.thread(&thread_id, e.clone());
    }
    // Workspace events of this batch (e.g. `interaction/pending`) come before the expiries
    // below: an interaction requested and expired in the same batch must be announced
    // before it is closed.
    for e in &uow.ws_events {
        em.workspace(e.clone());
    }
    for (iid, reason) in &uow.expire {
        if let Some(mut r) = store::get_interaction(tx, iid)?
            && r.interaction.status == InteractionStatus::Pending
        {
            r.interaction.status = InteractionStatus::Expired;
            r.interaction.resolved_at = Some(now);
            r.interaction.resolved_by = Some("system".into());
            r.interaction.expire_reason = Some(*reason);
            store::update_interaction(tx, &r.interaction)?;
            em.thread(
                &thread_id,
                Event::InteractionExpired {
                    interaction: r.interaction.clone(),
                },
            );
            em.workspace(Event::InteractionClosed {
                interaction_id: r.interaction.id.clone(),
                thread_id: thread_id.clone(),
                status: InteractionStatus::Expired,
            });
        }
    }
    if uow.queue_changed {
        let queued = store::list_queued(tx, &thread_id, preview_chars)?;
        em.thread(&thread_id, Event::QueueUpdated { queued });
    }
    let view = if uow.thread_changed || uow.thread_insert {
        Some(thread_changed(tx, em, &mut row)?)
    } else {
        None
    };
    for (idem, value) in &uow.idem_values {
        idem.store_result(tx, value)?;
    }
    if let Some(view) = &view {
        for idem in &uow.idem_thread {
            idem.store_result(
                tx,
                &ThreadResult {
                    thread: view.clone(),
                },
            )?;
        }
        if let Some((idem, started)) = &uow.idem_create {
            idem.store_result(
                tx,
                &ThreadCreateResult {
                    thread: view.clone(),
                    turn_id: started.turn_id.clone(),
                    disposition: Some(started.disposition),
                },
            )?;
        }
    }
    Ok((view, row.head))
}

impl Actor {
    async fn start_turn_request(
        &mut self,
        input: Vec<InputPart>,
        delivery: Delivery,
        idem: Option<Idem>,
        create: bool,
    ) -> CoreResult<TurnStartResult> {
        if self.row.archived || self.row.removed {
            return Err(invalid_state("the thread is archived"));
        }
        if !self.sh.accepts_work() {
            return Err(rpc(ErrorKind::Draining, "the server is shutting down"));
        }
        self.sh.registry.wait_ready().await;
        let info = self.harness_info()?;
        let converted = self.check_input(&input, &info).await?;
        if self.turn.is_none() {
            // The turn would start a fork's native session: refused while that would copy
            // turns of the parent the fork does not show.
            self.check_fork_point().await?;
        }
        let ConvertedInput {
            input: turn_input,
            attachments,
            mentions,
            text,
            pins,
        } = converted;

        if self.turn.is_some() {
            let uow = Uow {
                pins,
                ..Default::default()
            };
            return match delivery {
                Delivery::Steer => {
                    self.steer(turn_input, text, attachments, mentions, &info, idem, uow)
                        .await
                }
                Delivery::Auto | Delivery::Queue => self.enqueue(input, idem, uow).await,
            };
        }
        if create {
            // The new thread is inserted together with its first turn.
            let mut uow = Uow {
                thread_insert: true,
                pins,
                ..Default::default()
            };
            let result = self.open_turn(turn_input, text, attachments, mentions, &mut uow);
            uow.idem_create = idem.map(|i| (i, result.clone()));
            self.commit(uow).await?;
            self.begin_launch(None).await;
            return Ok(result);
        }
        let mut uow = Uow {
            pins,
            ..Default::default()
        };
        if self.row.queue_paused {
            self.row.queue_paused = false;
            uow.thread_changed = true;
        }
        let result = self.open_turn(turn_input, text, attachments, mentions, &mut uow);
        if let Some(idem) = idem {
            uow.idem_values.push((idem, serde_json::to_value(&result)?));
        }
        self.commit(uow).await?;
        self.begin_launch(None).await;
        Ok(result)
    }

    /// Creates the turn and the user's message item (in `uow`) and marks the turn active.
    fn open_turn(
        &mut self,
        input: TurnInput,
        text: String,
        attachments: Vec<Attachment>,
        mentions: Vec<Mention>,
        uow: &mut Uow,
    ) -> TurnStartResult {
        let now = now_ms();
        let index = self.next_turn_index;
        self.next_turn_index += 1;
        let turn = Turn {
            id: TurnId::generate(),
            thread_id: self.row.id.clone(),
            index,
            status: TurnStatus::Running,
            started_at: now,
            completed_at: None,
            model: self.row.settings.model.clone(),
            error: None,
            usage: None,
            diff: None,
            trigger: None,
        };
        let user_item = Item {
            id: ItemId::generate(),
            thread_id: self.row.id.clone(),
            turn_id: turn.id.clone(),
            status: ItemStatus::Completed,
            started_at: now,
            completed_at: Some(now),
            background_task_id: None,
            body: ItemBody::UserMessage {
                text: text.clone(),
                attachments,
                mentions,
                delivery: UserMessageDelivery::Normal,
            },
        };
        if self.row.title_source == "default" {
            let title = title_from(&text, self.sh.config.policy.first_message_title_chars);
            if !title.is_empty() {
                self.row.title = title;
                self.row.title_source = "firstMessage".into();
            }
        }
        self.row.last_activity_at = now;
        self.row.last_error = None;
        uow.thread_changed = true;
        uow.turn_inserts.push(TurnRow {
            turn: turn.clone(),
            base_tree: None,
            end_tree: None,
        });
        uow.events.push(Event::TurnStarted { turn: turn.clone() });
        uow.items_insert.push(user_item.clone());
        uow.events.push(Event::ItemStarted { item: user_item });
        let lease = self
            .sh
            .config
            .policy
            .prevent_sleep_while_running
            .then(|| self.sh.supervisor.power().acquire());
        self.sh.turn_started();
        self.last_turn = Some(turn.id.clone());
        self.turn = Some(ActiveTurn {
            id: turn.id.clone(),
            index,
            started_at: now,
            input,
            sent: false,
            items: HashMap::new(),
            order: Vec::new(),
            keys: HashMap::new(),
            usage: None,
            base_tree: None,
            _lease: lease,
            interrupt_requested: false,
            forced: false,
            trigger: None,
            background_notice: false,
        });
        TurnStartResult {
            disposition: Disposition::Started,
            turn_id: Some(turn.id),
            queued_id: None,
        }
    }

    /// A turn the agent started on its own (no user message, already running).
    fn open_agent_turn(&mut self, uow: &mut Uow) {
        let now = now_ms();
        let index = self.next_turn_index;
        self.next_turn_index += 1;
        let turn = Turn {
            id: TurnId::generate(),
            thread_id: self.row.id.clone(),
            index,
            status: TurnStatus::Running,
            started_at: now,
            completed_at: None,
            model: self
                .reported_model
                .clone()
                .or_else(|| self.row.settings.model.clone()),
            error: None,
            usage: None,
            diff: None,
            trigger: None,
        };
        self.row.last_activity_at = now;
        uow.thread_changed = true;
        uow.turn_inserts.push(TurnRow {
            turn: turn.clone(),
            base_tree: None,
            end_tree: None,
        });
        uow.events.push(Event::TurnStarted { turn: turn.clone() });
        let lease = self
            .sh
            .config
            .policy
            .prevent_sleep_while_running
            .then(|| self.sh.supervisor.power().acquire());
        self.sh.turn_started();
        self.last_turn = Some(turn.id.clone());
        self.turn = Some(ActiveTurn {
            id: turn.id,
            index,
            started_at: now,
            input: TurnInput::default(),
            sent: true,
            items: HashMap::new(),
            order: Vec::new(),
            keys: HashMap::new(),
            usage: None,
            base_tree: None,
            _lease: lease,
            interrupt_requested: false,
            forced: false,
            trigger: None,
            background_notice: false,
        });
        tracing::info!(thread = %self.row.id, "agent started a turn by itself");
    }

    /// Injects input into the running turn. `uow` may already hold changes that commit with the
    /// steer (a queued entry that is sent now).
    // The caller has already split the request into these parts (and holds the unit of work);
    // a one-off struct for this private method would only move the same list elsewhere.
    #[allow(clippy::too_many_arguments)]
    async fn steer(
        &mut self,
        input: TurnInput,
        text: String,
        attachments: Vec<Attachment>,
        mentions: Vec<Mention>,
        info: &aas_harness::HarnessInfo,
        idem: Option<Idem>,
        mut uow: Uow,
    ) -> CoreResult<TurnStartResult> {
        if !info.capabilities.steer {
            return Err(rpc(
                ErrorKind::CapabilityUnsupported,
                "this harness cannot steer a running turn",
            )
            .with("capability", "steer"));
        }
        let deadline = self.request_deadline();
        let turn = self.turn.as_mut().expect("steer requires a turn");
        if turn.sent {
            let control = self
                .live
                .as_ref()
                .map(|l| l.control.clone())
                .ok_or_else(|| invalid_state("the agent is not running"))?;
            bounded(deadline, "the steer", control.steer(input))
                .await
                .map_err(adapter_err(&self.row.harness_id))?;
        } else {
            // Not sent yet: the steer becomes part of the turn's first message.
            turn.input.parts.push(TurnInputPart::Text("\n\n".into()));
            turn.input.parts.extend(input.parts);
        }
        let turn = self.turn.as_ref().expect("turn");
        let now = now_ms();
        let item = Item {
            id: ItemId::generate(),
            thread_id: self.row.id.clone(),
            turn_id: turn.id.clone(),
            status: ItemStatus::Completed,
            started_at: now,
            completed_at: Some(now),
            background_task_id: None,
            body: ItemBody::UserMessage {
                text,
                attachments,
                mentions,
                delivery: UserMessageDelivery::Steer,
            },
        };
        let result = TurnStartResult {
            disposition: Disposition::Steered,
            turn_id: Some(turn.id.clone()),
            queued_id: None,
        };
        uow.items_insert.push(item.clone());
        uow.events.push(Event::ItemStarted { item });
        if let Some(idem) = idem {
            uow.idem_values.push((idem, serde_json::to_value(&result)?));
        }
        self.row.last_activity_at = now;
        uow.thread_changed = true;
        self.commit(uow).await?;
        Ok(result)
    }

    async fn enqueue(
        &mut self,
        input: Vec<InputPart>,
        idem: Option<Idem>,
        mut uow: Uow,
    ) -> CoreResult<TurnStartResult> {
        let q = QueuedInput {
            id: QueuedInputId::generate(),
            thread_id: self.row.id.clone(),
            created_at: now_ms(),
            preview: store::input_preview(&input, self.sh.config.policy.queued_preview_chars),
            input,
        };
        let result = TurnStartResult {
            disposition: Disposition::Queued,
            turn_id: None,
            queued_id: Some(q.id.clone()),
        };
        uow.queue_changed = true;
        uow.thread_changed = true;
        uow.queued_insert.push(q);
        if let Some(idem) = idem {
            uow.idem_values.push((idem, serde_json::to_value(&result)?));
        }
        self.queue_len += 1;
        if let Err(e) = self.commit(uow).await {
            self.queue_len -= 1;
            return Err(e);
        }
        Ok(result)
    }

    async fn remove_queued(
        &mut self,
        queued_id: QueuedInputId,
        idem: Option<Idem>,
    ) -> CoreResult<QueueRemoveResult> {
        let thread_id = self.row.id.clone();
        let preview_chars = self.sh.config.policy.queued_preview_chars;
        let queued = self
            .sh
            .db
            .read(move |tx| store::list_queued(tx, &thread_id, preview_chars))
            .await?;
        let removed = queued.iter().any(|q| q.id == queued_id);
        let result = QueueRemoveResult { removed };
        let mut uow = Uow::default();
        if removed {
            uow.queued_delete.push(queued_id);
            uow.queue_changed = true;
            uow.thread_changed = true;
            self.queue_len = self.queue_len.saturating_sub(1);
            if self.queue_len == 0 && self.row.queue_paused {
                self.row.queue_paused = false;
            }
        }
        if let Some(idem) = idem {
            uow.idem_values.push((idem, serde_json::to_value(&result)?));
        }
        self.commit(uow).await?;
        Ok(result)
    }

    async fn resume_queue(&mut self, idem: Option<Idem>) -> CoreResult<QueueResumeResult> {
        // Everything that can refuse the request runs before the queue's bookkeeping changes:
        // a refused resume (resent later by the client) leaves the queue exactly as it was.
        let mut next = None;
        if self.turn.is_none() && self.queue_len > 0 && self.sh.accepts_work() {
            let thread_id = self.row.id.clone();
            let preview_chars = self.sh.config.policy.queued_preview_chars;
            let queued = self
                .sh
                .db
                .read(move |tx| store::list_queued(tx, &thread_id, preview_chars))
                .await?;
            match queued.first() {
                Some(first) => {
                    self.sh.registry.wait_ready().await;
                    self.harness_info()?;
                    let converted = self.convert_input(&first.input).await?;
                    self.check_fork_point().await?;
                    next = Some((first.id.clone(), converted, queued.len() - 1));
                }
                None => self.queue_len = 0,
            }
        }
        let mut uow = Uow::default();
        if self.row.queue_paused {
            self.row.queue_paused = false;
            uow.thread_changed = true;
        }
        let mut turn_id = None;
        if let Some((id, c, remaining)) = next {
            self.queue_len = remaining;
            uow.pins.extend(c.pins);
            uow.queued_delete.push(id);
            uow.queue_changed = true;
            let r = self.open_turn(c.input, c.text, c.attachments, c.mentions, &mut uow);
            turn_id = r.turn_id;
        }
        let result = QueueResumeResult {
            turn_id: turn_id.clone(),
        };
        if let Some(idem) = idem {
            uow.idem_values.push((idem, serde_json::to_value(&result)?));
        }
        self.commit(uow).await?;
        if turn_id.is_some() {
            self.begin_launch(None).await;
        }
        Ok(result)
    }

    async fn update_queued(
        &mut self,
        queued_id: QueuedInputId,
        input: Vec<InputPart>,
        idem: Option<Idem>,
    ) -> CoreResult<QueueUpdateResult> {
        // Validated like a `turn/start` input: the entry must be sendable as it is.
        self.sh.registry.wait_ready().await;
        let info = self.harness_info()?;
        let converted = self.check_input(&input, &info).await?;
        let thread_id = self.row.id.clone();
        let preview_chars = self.sh.config.policy.queued_preview_chars;
        let queued = self
            .sh
            .db
            .read(move |tx| store::list_queued(tx, &thread_id, preview_chars))
            .await?;
        let updated = queued.iter().any(|q| q.id == queued_id);
        let result = QueueUpdateResult { updated };
        let mut uow = Uow {
            pins: converted.pins,
            ..Default::default()
        };
        if updated {
            uow.queued_update.push((queued_id, input));
            uow.queue_changed = true;
        }
        if let Some(idem) = idem {
            uow.idem_values.push((idem, serde_json::to_value(&result)?));
        }
        self.commit(uow).await?;
        Ok(result)
    }

    /// "Send now" for a queued entry: steered into the running turn, or started as a new turn
    /// when none runs. The entry leaves the queue in the same commit.
    async fn steer_queued(
        &mut self,
        queued_id: QueuedInputId,
        idem: Option<Idem>,
    ) -> CoreResult<QueueSteerResult> {
        let thread_id = self.row.id.clone();
        let preview_chars = self.sh.config.policy.queued_preview_chars;
        let queued = self
            .sh
            .db
            .read(move |tx| store::list_queued(tx, &thread_id, preview_chars))
            .await?;
        let Some(entry) = queued.iter().find(|q| q.id == queued_id).cloned() else {
            let result = QueueSteerResult {
                disposition: None,
                turn_id: None,
            };
            let mut uow = Uow::default();
            if let Some(idem) = idem {
                uow.idem_values.push((idem, serde_json::to_value(&result)?));
            }
            self.commit(uow).await?;
            return Ok(result);
        };
        if self.row.archived || self.row.removed {
            return Err(invalid_state("the thread is archived"));
        }
        if !self.sh.accepts_work() {
            return Err(rpc(ErrorKind::Draining, "the server is shutting down"));
        }
        self.sh.registry.wait_ready().await;
        let info = self.harness_info()?;
        let converted = self.check_input(&entry.input, &info).await?;
        if self.turn.is_none() {
            self.check_fork_point().await?;
        }
        let ConvertedInput {
            input,
            attachments,
            mentions,
            text,
            pins,
        } = converted;
        let remaining = queued.len() - 1;
        let mut uow = Uow {
            queue_changed: true,
            thread_changed: true,
            pins,
            ..Default::default()
        };
        uow.queued_delete.push(entry.id.clone());
        let queue_paused_before = self.row.queue_paused;
        if let Some(turn) = &self.turn {
            let result = QueueSteerResult {
                disposition: Some(Disposition::Steered),
                turn_id: Some(turn.id.clone()),
            };
            if let Some(idem) = idem {
                uow.idem_values.push((idem, serde_json::to_value(&result)?));
            }
            if remaining == 0 {
                self.row.queue_paused = false;
            }
            if let Err(e) = self
                .steer(input, text, attachments, mentions, &info, None, uow)
                .await
            {
                self.row.queue_paused = queue_paused_before;
                return Err(e);
            }
            self.queue_len = remaining;
            return Ok(result);
        }
        // No turn runs: the entry starts one, like a `turn/start` (which also resumes a paused
        // queue).
        self.row.queue_paused = false;
        let started = self.open_turn(input, text, attachments, mentions, &mut uow);
        let result = QueueSteerResult {
            disposition: Some(Disposition::Started),
            turn_id: started.turn_id,
        };
        if let Some(idem) = idem {
            uow.idem_values.push((idem, serde_json::to_value(&result)?));
        }
        self.queue_len = remaining;
        self.commit(uow).await?;
        self.begin_launch(None).await;
        Ok(result)
    }

    async fn pop_queued(&mut self) -> CoreResult<Option<QueuedInput>> {
        let thread_id = self.row.id.clone();
        let preview_chars = self.sh.config.policy.queued_preview_chars;
        let mut queued = self
            .sh
            .db
            .read(move |tx| store::list_queued(tx, &thread_id, preview_chars))
            .await?;
        if queued.is_empty() {
            self.queue_len = 0;
            return Ok(None);
        }
        self.queue_len = queued.len() - 1;
        Ok(Some(queued.remove(0)))
    }

    async fn interrupt(&mut self, idem: Option<Idem>) -> CoreResult<TurnInterruptResult> {
        let mut uow = Uow::default();
        let not_started = || {
            Some(TurnError {
                message: "interrupted before the agent started".into(),
                kind: "interrupted".into(),
            })
        };
        let mut interrupted = false;
        // A user turn waiting behind an agent-initiated run never reached the agent.
        if let Some(turn) = self.deferred.take() {
            self.close_turn(
                turn,
                TurnStatus::Interrupted,
                None,
                not_started(),
                ExpireReason::TurnEnded,
                &mut uow,
            );
            interrupted = true;
        }
        if self.turn.as_ref().is_some_and(|t| !t.sent) {
            // Not sent to the agent yet: cancel the launch.
            self.cancel_launch();
            self.finish_turn(
                TurnStatus::Interrupted,
                None,
                not_started(),
                ExpireReason::TurnEnded,
                &mut uow,
            );
            interrupted = true;
        } else if let Some(turn) = self.turn.as_mut() {
            if !turn.interrupt_requested {
                turn.interrupt_requested = true;
                // The grace runs from the user's request, not from the adapter's answer: the
                // deadline is armed first, and the request to the CLI is bounded by it. A CLI
                // that does not even acknowledge the interrupt is stopped when it expires,
                // right after this request is answered (`on_interrupt_deadline`).
                let deadline = Instant::now() + self.sh.config.policy.interrupt_grace;
                self.interrupt_deadline = Some(deadline);
                if let Some(control) = self.live.as_ref().map(|l| l.control.clone())
                    && let Err(e) = bounded(deadline, "the interrupt", control.interrupt()).await
                {
                    tracing::warn!(thread = %self.row.id, error = %e, "interrupt request failed; the process is stopped unless the turn ends within the grace");
                }
            }
            interrupted = true;
        }
        let result = TurnInterruptResult { interrupted };
        if let Some(idem) = idem {
            uow.idem_values.push((idem, serde_json::to_value(&result)?));
        }
        let after = self.after_turn_end(&mut uow).await;
        self.commit(uow).await?;
        self.run_after(after).await;
        Ok(result)
    }

    async fn respond(
        &mut self,
        interaction_id: InteractionId,
        resolution: InteractionResolution,
        device: DeviceId,
        idem: Option<Idem>,
    ) -> CoreResult<InteractionRespondResult> {
        let id = interaction_id.clone();
        let row = self
            .sh
            .db
            .read(move |tx| store::get_interaction(tx, &id))
            .await?;
        let Some(row) = row else {
            return Err(not_found("interaction", &interaction_id));
        };
        let mut interaction = row.interaction;
        if interaction.status != InteractionStatus::Pending {
            let result = InteractionRespondResult {
                interaction,
                already_resolved: true,
            };
            if let Some(idem) = idem {
                let value = serde_json::to_value(&result)?;
                let mut uow = Uow::default();
                uow.idem_values.push((idem, value));
                self.commit(uow).await?;
            }
            return Ok(result);
        }
        validate_resolution(&interaction.request, &resolution)?;
        let control = self.live.as_ref().map(|l| l.control.clone());
        let mut uow = Uow::default();
        let now = now_ms();
        let deadline = self.request_deadline();
        match control {
            Some(control) => match bounded(
                deadline,
                "the answer",
                control.respond(&row.adapter_request_id, &resolution),
            )
            .await
            {
                Ok(()) => {
                    interaction.status = InteractionStatus::Resolved;
                    interaction.resolved_at = Some(now);
                    interaction.resolved_by = Some(device.into_string());
                    interaction.resolution = Some(resolution);
                    uow.interactions_update.push(interaction.clone());
                    uow.events.push(Event::InteractionResolved {
                        interaction: interaction.clone(),
                    });
                    uow.ws_events.push(Event::InteractionClosed {
                        interaction_id: interaction.id.clone(),
                        thread_id: self.row.id.clone(),
                        status: InteractionStatus::Resolved,
                    });
                    self.pending.retain(|_, p| p.id != interaction.id);
                }
                Err(e @ (AdapterError::Closed | AdapterError::UnknownRequest(_))) => {
                    // The process is gone, or the agent no longer waits for this request (it
                    // withdrew it): nothing can take the answer.
                    let reason = match e {
                        AdapterError::Closed => ExpireReason::ProcessExited,
                        _ => ExpireReason::HarnessCancelled,
                    };
                    self.pending.retain(|_, p| p.id != interaction.id);
                    uow.expire.push((interaction.id.clone(), reason));
                    interaction.status = InteractionStatus::Expired;
                    interaction.expire_reason = Some(reason);
                    interaction.resolved_at = Some(now);
                    interaction.resolved_by = Some("system".into());
                }
                Err(e) => return Err(adapter_err(&self.row.harness_id)(e)),
            },
            None => {
                uow.expire
                    .push((interaction.id.clone(), ExpireReason::ProcessExited));
                interaction.status = InteractionStatus::Expired;
                interaction.expire_reason = Some(ExpireReason::ProcessExited);
                interaction.resolved_at = Some(now);
                interaction.resolved_by = Some("system".into());
            }
        }
        uow.thread_changed = true;
        let result = InteractionRespondResult {
            interaction,
            already_resolved: false,
        };
        if let Some(idem) = idem {
            uow.idem_values.push((idem, serde_json::to_value(&result)?));
        }
        self.commit(uow).await?;
        Ok(result)
    }

    async fn update(
        &mut self,
        title: Option<String>,
        settings: Option<ThreadSettings>,
        pinned: Option<bool>,
        idem: Option<Idem>,
    ) -> CoreResult<ThreadUpdateResult> {
        // Everything that can refuse the request (validation, the process refusing the
        // settings) runs before the thread changes: a refused update must leave no trace,
        // since every later commit persists and publishes the thread's row.
        let title = match title {
            Some(title) => {
                let title = title.trim().to_owned();
                if title.is_empty() {
                    return Err(invalid_params("title must not be empty"));
                }
                Some(title)
            }
            None => None,
        };
        let mut outcome = None;
        let mut new_settings = None;
        if let Some(s) = settings {
            // Only the values this request sets are checked, and only against the lists of an
            // available harness (design.md §5.4): the thread's other values were accepted when
            // they were set, and the placeholder information of an unavailable harness lists
            // nothing, which says nothing about which values are valid. The request must not
            // be refused for good (`invalidParams` is stored) for either reason.
            if let Some(info) = self.sh.registry.info(&self.row.harness_id)
                && info.available
            {
                validate_settings(&info, &s)?;
            }
            let merged = ThreadSettings {
                model: s.model.or_else(|| self.row.settings.model.clone()),
                effort: s.effort.or_else(|| self.row.settings.effort.clone()),
                permission_mode: s
                    .permission_mode
                    .or_else(|| self.row.settings.permission_mode.clone()),
            };
            outcome = Some(self.settings_outcome(&merged).await?);
            new_settings = Some(merged);
        }
        if let Some(title) = title {
            self.row.title = title;
            self.row.title_source = "user".into();
        }
        if let Some(pinned) = pinned {
            self.row.pinned = pinned;
        }
        if let Some(settings) = new_settings {
            self.row.settings = settings;
        }
        let mut uow = Uow {
            thread_changed: true,
            ..Default::default()
        };
        let idem2 = idem.clone();
        let view = {
            let v = self.commit(std::mem::take(&mut uow)).await?;
            match v {
                Some(v) => v,
                None => self.current_view().await?,
            }
        };
        let result = ThreadUpdateResult {
            thread: view,
            settings_outcome: outcome,
        };
        if let Some(idem) = idem2 {
            let value = serde_json::to_value(&result)?;
            let mut uow = Uow::default();
            uow.idem_values.push((idem, value));
            self.commit(uow).await?;
        }
        Ok(result)
    }

    /// How the settings `merged` reach the agent (`thread/update`, design.md §5.4). Called
    /// before the thread's settings change; an idle process is asked to apply them now.
    ///
    /// * No process: the next one starts with them (a start in progress applies them before
    ///   its turn is sent, see `on_launched`): `appliesNextTurn`, or `appliedLive` when they
    ///   do not change.
    /// * The process already runs with them: `appliedLive`.
    /// * A turn runs (or the process is being stopped): nothing is sent now, since a running
    ///   turn is never affected; they are applied before the next turn is sent:
    ///   `appliesNextTurn`.
    /// * Otherwise the process applies them now (`appliedLive`), or needs a restart, which the
    ///   next turn does (`appliesNextTurn`). When it fails, the request fails and the process
    ///   is replaced before the next turn: it may have applied a part of them.
    async fn settings_outcome(&mut self, merged: &ThreadSettings) -> CoreResult<SettingsOutcome> {
        let Some(live) = &self.live else {
            return Ok(if *merged == self.row.settings {
                SettingsOutcome::AppliedLive
            } else {
                SettingsOutcome::AppliesNextTurn
            });
        };
        if live.settings == *merged {
            return Ok(SettingsOutcome::AppliedLive);
        }
        if self.turn.as_ref().is_some_and(|t| t.sent) || self.stopping.is_some() {
            return Ok(SettingsOutcome::AppliesNextTurn);
        }
        let control = live.control.clone();
        let deadline = self.request_deadline();
        match bounded(deadline, "the settings", control.apply_settings(merged)).await {
            Ok(SettingsApplied::Live) => {
                if let Some(live) = self.live.as_mut() {
                    live.settings = merged.clone();
                }
                Ok(SettingsOutcome::AppliedLive)
            }
            Ok(SettingsApplied::RequiresRestart) => {
                self.restart_pending = true;
                Ok(SettingsOutcome::AppliesNextTurn)
            }
            Err(e) => {
                self.restart_pending = true;
                Err(adapter_err(&self.row.harness_id)(e))
            }
        }
    }

    async fn stop(&mut self, idem: Option<Idem>, reply: Reply<ThreadResult>) {
        let mut uow = Uow::default();
        self.end_unsent_turns(
            TurnError {
                message: "stopped by the user".into(),
                kind: "stopped".into(),
            },
            &mut uow,
        );
        if self.live.is_some() || self.starting_process() {
            if self.stopping.is_none() {
                self.begin_stop(StopReason::User, &mut uow);
            }
            if let Some(s) = &mut self.stopping {
                s.waiters.push((idem, reply));
            }
            self.commit_logged(uow).await;
            return;
        }
        if let Some(idem) = idem {
            uow.idem_thread.push(idem);
        }
        uow.thread_changed = true;
        let r = match self.commit(uow).await {
            Ok(Some(view)) => Ok(ThreadResult { thread: view }),
            Ok(None) => self
                .current_view()
                .await
                .map(|thread| ThreadResult { thread }),
            Err(e) => Err(e),
        };
        send_reply(reply, r);
    }

    async fn archive(&mut self, req: ArchiveReq, idem: Option<Idem>, reply: Reply<ThreadResult>) {
        if req.archived && req.remove_worktree {
            // Refuse before stopping anything.
            if let Err(e) = self.check_worktree_not_shared().await {
                send_reply(reply, Err(e));
                return;
            }
        }
        let busy = self.live.is_some()
            || self.launch.is_some()
            || self.turn.is_some()
            || self.deferred.is_some();
        if req.archived && busy {
            // Stop first; archiving continues when the process has exited.
            let mut uow = Uow::default();
            self.end_unsent_turns(
                TurnError {
                    message: "thread archived".into(),
                    kind: "stopped".into(),
                },
                &mut uow,
            );
            if self.live.is_some() || self.starting_process() {
                if self.stopping.is_none() {
                    self.begin_stop(StopReason::User, &mut uow);
                }
                if let Some(s) = &mut self.stopping {
                    s.archive.push((req, idem, reply));
                }
                self.commit_logged(uow).await;
                return;
            }
            self.commit_logged(uow).await;
        }
        let r = self.apply_archive(req, idem).await;
        send_reply(reply, r);
    }

    /// Fails when another thread (not removed) works in this thread's worktree: removing the
    /// worktree would delete that thread's folder (forks share the worktree of their parent).
    fn check_worktree_not_shared(&self) -> impl Future<Output = CoreResult<()>> + Send + 'static {
        let worktree = match &self.row.workspace {
            Workspace::Worktree { path, .. } => Some(crate::config::path_key(Path::new(path))),
            Workspace::Local => None,
        };
        let (db, project_id, me) = (
            self.sh.db.clone(),
            self.row.project_id.clone(),
            self.row.id.clone(),
        );
        async move {
            let Some(key) = worktree else { return Ok(()) };
            let threads = db
                .read(move |tx| store::threads_of_project(tx, &project_id))
                .await?;
            let sharing: Vec<String> = threads
                .iter()
                .filter(|t| t.id != me)
                .filter(|t| matches!(&t.workspace, Workspace::Worktree { path, .. } if crate::config::path_key(Path::new(path)) == key))
                .map(|t| t.id.to_string())
                .collect();
            if sharing.is_empty() {
                Ok(())
            } else {
                Err(invalid_state(format!(
                    "the worktree is also used by thread {}; archive without removing the worktree, or remove that thread first",
                    sharing.join(", ")
                )))
            }
        }
    }

    async fn apply_archive(
        &mut self,
        req: ArchiveReq,
        idem: Option<Idem>,
    ) -> CoreResult<ThreadResult> {
        if req.archived
            && req.remove_worktree
            && let Workspace::Worktree { path, .. } = &self.row.workspace
        {
            self.check_worktree_not_shared().await?;
            let git = self
                .sh
                .git
                .clone()
                .ok_or_else(|| invalid_state("git is not available"))?;
            let project_id = self.row.project_id.clone();
            let project = self
                .sh
                .db
                .read(move |tx| store::get_project(tx, &project_id))
                .await?;
            let repo = project
                .map(|p| PathBuf::from(p.project.path))
                .unwrap_or_else(|| PathBuf::from(path));
            if Path::new(path).exists() {
                git.worktree_remove(&repo, Path::new(path), req.force)
                    .await?;
            }
            // The snapshots were taken in the removed folder: nothing can diff them any
            // more. A failure is left to maintenance, which retries until it succeeds.
            if let Err(e) = git.drop_snapshots(&repo, self.row.id.as_str()).await {
                tracing::warn!(thread = %self.row.id, error = %e, "could not release the turn snapshots; retried later");
                let (repo, thread) = (repo.display().to_string(), self.row.id.to_string());
                self.sh
                    .tx_durable("snapshot cleanup job", move |tx, _em| {
                        store::insert_cleanup_job(
                            tx,
                            store::CleanupKind::SnapshotRefs,
                            &repo,
                            &thread,
                            now_ms(),
                        )
                        .map(|_| ())
                    })
                    .await?;
            }
        }
        self.row.archived = req.archived;
        let mut uow = Uow {
            thread_changed: true,
            ..Default::default()
        };
        if let Some(idem) = idem {
            uow.idem_thread.push(idem);
        }
        match self.commit(uow).await? {
            Some(view) => Ok(ThreadResult { thread: view }),
            None => self
                .current_view()
                .await
                .map(|thread| ThreadResult { thread }),
        }
    }

    /// Stops the process (if any); the actor exits once it is gone and then answers every
    /// waiter (`reply` is `None` when the shutdown comes from the mailbox closing).
    async fn on_shutdown(&mut self, reply: Option<oneshot::Sender<()>>) {
        if self.retired {
            // Nothing runs and nothing may be written.
            self.shutting_down = true;
            self.exit_waiters.extend(reply);
            return;
        }
        let mut uow = Uow::default();
        self.end_unsent_turns(shutdown_error(&self.sh), &mut uow);
        if (self.live.is_some() || self.starting_process()) && self.stopping.is_none() {
            self.begin_stop(StopReason::Shutdown, &mut uow);
        }
        self.commit_logged(uow).await;
        self.shutting_down = true;
        self.exit_waiters.extend(reply);
    }

    // ----- process lifecycle -------------------------------------------------------------------

    /// Starts stopping the thread's process. Without a process yet (a start in progress), the
    /// process is stopped as soon as it exists (`on_launched`). The stop completes in
    /// `settle_stop`.
    fn begin_stop(&mut self, reason: StopReason, uow: &mut Uow) {
        // A stop ends the thread's work: queued inputs wait for `queue/resume` instead of
        // starting on a new process behind the user's back.
        if matches!(reason, StopReason::User | StopReason::Shutdown)
            && self.queue_len > 0
            && !self.row.queue_paused
        {
            self.row.queue_paused = true;
            uow.thread_changed = true;
        }
        self.stopping = Some(Stopping {
            reason,
            waiters: Vec::new(),
            archive: Vec::new(),
        });
        self.idle_deadline = None;
        self.stop_live_process(reason);
    }

    fn stop_live_process(&self, reason: StopReason) {
        if let Some(live) = &self.live {
            let control = live.control.clone();
            tokio::spawn(async move {
                control.shutdown(reason).await;
            });
        }
    }

    async fn on_idle_deadline(&mut self) {
        self.idle_deadline = None;
        if self.reapable() {
            tracing::info!(thread = %self.row.id, "stopping idle agent process");
            let mut uow = Uow::default();
            self.begin_stop(StopReason::Idle, &mut uow);
            self.commit_logged(uow).await;
        }
    }

    async fn on_interrupt_deadline(&mut self) {
        self.interrupt_deadline = None;
        let live = self.live.is_some();
        let idle_stop = self.stopping.is_none();
        let forced = match self.turn.as_mut() {
            Some(turn) if turn.sent && turn.interrupt_requested && live && idle_stop => {
                turn.forced = true;
                true
            }
            _ => false,
        };
        if forced && self.background.busy() > 0 {
            // Stopping the process would kill the background work it runs, and work the
            // harness reports as running is never killed because of time (design.md §4.3):
            // the turn goes on; the user can interrupt again or stop the thread.
            let mut uow = Uow::default();
            if let Some(turn) = self.turn.as_mut() {
                turn.forced = false;
                turn.interrupt_requested = false;
                let turn_id = turn.id.clone();
                tracing::warn!(thread = %self.row.id, "agent did not honour the interrupt; its process is kept for its background work");
                self.core_notice(
                    &turn_id,
                    NoticeLevel::Warning,
                    "The agent did not stop the turn in time. Its process is kept because it runs background work; interrupt again, or stop the thread to stop everything.",
                    "interruptNotHonoured",
                    &mut uow,
                );
            }
            self.commit_logged(uow).await;
            return;
        }
        if forced {
            tracing::warn!(thread = %self.row.id, "agent did not honour the interrupt; stopping its process");
            let mut uow = Uow::default();
            self.begin_stop(StopReason::InterruptTimeout, &mut uow);
            self.commit_logged(uow).await;
        }
    }

    /// A process start (not just a snapshot) is in progress.
    fn starting_process(&self) -> bool {
        self.launch
            .as_ref()
            .is_some_and(|l| matches!(l.phase, Phase::Launching(_)) && l.kind != LaunchKind::Reuse)
    }

    /// The turn a launch was started for no longer needs it. Waiting for capacity and taking
    /// the snapshot for a live process are simply dropped. A process start in progress is not:
    /// dropping it midway would orphan the process (and free its permit while it runs), and a
    /// restart may be shutting the old process down. It keeps running, skips the start if it
    /// has not begun yet, and what it produces is kept as ready — or stopped when the thread is
    /// being stopped (`on_launched`).
    fn cancel_launch(&mut self) {
        let Some(launch) = &self.launch else { return };
        match (&launch.phase, launch.kind) {
            (Phase::Capacity(_), _) | (Phase::Launching(_), LaunchKind::Reuse) => {
                self.launch = None
            }
            (Phase::Launching(_), _) => launch.abort.store(true, Ordering::SeqCst),
        }
    }

    /// Ends the turns that never reached the agent (and cancels their launch) because the
    /// thread is being stopped, archived or shut down.
    fn end_unsent_turns(&mut self, error: TurnError, uow: &mut Uow) {
        self.cancel_launch();
        if let Some(turn) = self.deferred.take() {
            self.close_turn(
                turn,
                TurnStatus::Interrupted,
                None,
                Some(error.clone()),
                ExpireReason::TurnEnded,
                uow,
            );
        }
        if self.turn.as_ref().is_some_and(|t| !t.sent) {
            self.finish_turn(
                TurnStatus::Interrupted,
                None,
                Some(error),
                ExpireReason::TurnEnded,
                uow,
            );
        }
    }

    /// Starts what the current turn needs before its input can be sent: its base snapshot,
    /// and a process when none runs (or a new one when settings require a restart). `base` is
    /// the end snapshot of a turn that ended right before (only then is it still current).
    async fn begin_launch(&mut self, base: Option<String>) {
        let Some(turn_id) = self.turn.as_ref().filter(|t| !t.sent).map(|t| t.id.clone()) else {
            return;
        };
        if self.stopping.is_some() {
            // The process is being stopped: the turn gets a new process once the old one has
            // exited (`settle_stop`), never the dying one.
            self.commit_logged(Uow::default()).await;
            return;
        }
        if let Some(launch) = &self.launch {
            // A launch started for an earlier turn is still in progress: its outcome serves
            // this turn too (`on_launched`).
            launch.abort.store(false, Ordering::SeqCst);
            return;
        }
        let settings = self.row.settings.clone();
        let abort = Arc::new(AtomicBool::new(false));
        let restart = self.restart_pending && self.live.is_some();
        let mut uow = Uow::default();
        if restart && self.background.busy() > 0 {
            // The process must be replaced for the thread's settings, but background work
            // keeps it busy, and that work is never killed for a restart: the turn waits until
            // the work has ended (or the user stops it; `resume_after_background`).
            self.note_waiting_for_background(&mut uow);
            self.commit_logged(uow).await;
            return;
        }
        self.restart_pending = false;
        let (kind, phase) = if restart {
            // What the old process still ran (ambient work) ends with it.
            self.end_background(
                ProcessEnd::Stopped(BackgroundEndReason::ProcessReplaced),
                &mut uow,
            );
            // Settings require a new process: reuse the old process's capacity permit
            // (waiting for a new one while holding it could deadlock at capacity 1).
            let old = self.live.take().expect("live session");
            let fut = self.launching_future(
                Some(old.permit),
                Some(old.control),
                base,
                settings.clone(),
                abort.clone(),
            );
            (LaunchKind::Restart, Phase::Launching(fut))
        } else if self.live.is_none() {
            match self.sh.capacity.try_acquire() {
                Some(permit) => {
                    let fut = self.launching_future(
                        Some(permit),
                        None,
                        base,
                        settings.clone(),
                        abort.clone(),
                    );
                    (LaunchKind::Fresh, Phase::Launching(fut))
                }
                None => {
                    let cap = self.sh.capacity.clone();
                    (
                        LaunchKind::Fresh,
                        Phase::Capacity(Box::pin(async move { cap.acquire().await })),
                    )
                }
            }
        } else {
            let fut = self.launching_future(None, None, base, settings.clone(), abort.clone());
            (LaunchKind::Reuse, Phase::Launching(fut))
        };
        self.launch = Some(Launch {
            phase,
            turn: turn_id,
            kind,
            settings,
            abort,
        });
        self.commit_logged(uow).await;
    }

    /// Adds a notice to the unsent turn (once) saying that it waits for background work: the
    /// process must be replaced to apply the thread's settings first.
    fn note_waiting_for_background(&mut self, uow: &mut Uow) {
        let Some(turn) = self
            .turn
            .as_mut()
            .filter(|t| !t.sent && !t.background_notice)
        else {
            return;
        };
        turn.background_notice = true;
        let turn_id = turn.id.clone();
        tracing::info!(thread = %self.row.id, "the next turn waits for background work before the process is replaced");
        self.core_notice(
            &turn_id,
            NoticeLevel::Info,
            "The agent must be restarted to apply the thread's new settings. The turn starts when its background work has ended; stop the background work to start it now.",
            "waitingForBackgroundWork",
            uow,
        );
    }

    /// A notice item the engine adds to turn `turn_id` (complete from the start).
    fn core_notice(
        &mut self,
        turn_id: &TurnId,
        level: NoticeLevel,
        message: &str,
        code: &str,
        uow: &mut Uow,
    ) {
        let now = now_ms();
        let item = Item {
            id: ItemId::generate(),
            thread_id: self.row.id.clone(),
            turn_id: turn_id.clone(),
            status: ItemStatus::Completed,
            started_at: now,
            completed_at: Some(now),
            background_task_id: None,
            body: ItemBody::Notice {
                level,
                message: message.into(),
                code: Some(code.into()),
            },
        };
        uow.items_insert.push(item.clone());
        uow.events.push(Event::ItemStarted { item });
    }

    /// A turn that waited for background work before the process could be replaced (see
    /// [`note_waiting_for_background`](Self::note_waiting_for_background)) starts once no
    /// background work keeps the agent busy any more.
    async fn resume_after_background(&mut self) {
        let waiting = self.restart_pending
            && self.live.is_some()
            && self.launch.is_none()
            && self.stopping.is_none()
            && self.turn.as_ref().is_some_and(|t| !t.sent)
            && self.background.busy() == 0;
        if waiting {
            self.begin_launch(None).await;
        }
    }

    fn launching_future(
        &mut self,
        permit: Option<Permit>,
        old: Option<Arc<dyn SessionControl>>,
        base: Option<String>,
        settings: ThreadSettings,
        abort: Arc<AtomicBool>,
    ) -> Pin<Box<dyn Future<Output = LaunchResult> + Send>> {
        let git = self.sh.git.clone();
        let cwd = PathBuf::from(&self.row.cwd);
        let thread = self.row.id.to_string();
        let adapter = self.sh.registry.get(&self.row.harness_id);
        let mode = match (&self.row.fork_source, &self.row.native_session_id) {
            (Some(src), _) => StartMode::Fork {
                native_session_id: src.clone(),
            },
            (None, Some(native)) => StartMode::Resume {
                native_session_id: native.clone(),
            },
            (None, None) => StartMode::New,
        };
        // A fork's parent may run turns while this start waits for capacity: checked again
        // right before the native session is branched off.
        let fork_check = match &mode {
            StartMode::Fork { .. } => self
                .row
                .forked_from
                .clone()
                .map(|origin| (self.sh.db.clone(), origin)),
            _ => None,
        };
        let req = StartRequest {
            thread_id: self.row.id.clone(),
            cwd: cwd.clone(),
            settings,
            mode,
        };
        Box::pin(async move {
            if let Some(old) = old {
                old.shutdown(StopReason::Idle).await;
            }
            if abort.load(Ordering::SeqCst) {
                // Nothing needs the process any more (the permit is released here).
                return LaunchResult {
                    base_tree: None,
                    session: None,
                    refused: None,
                };
            }
            let base_tree = match (base, git) {
                (Some(base), _) => Some(base),
                (None, Some(g)) => match g.snapshot_tree(&cwd, Some(&thread)).await {
                    Ok(tree) => tree,
                    Err(e) => {
                        tracing::warn!(cwd = %cwd.display(), error = %e, "git snapshot failed; this turn has no diff");
                        None
                    }
                },
                (None, None) => None,
            };
            if permit.is_some()
                && let Some((db, origin)) = fork_check
            {
                match fork_point_passed(&db, &origin).await {
                    Ok(false) => {}
                    Ok(true) => {
                        let refused = TurnError {
                            message: FORK_OUTDATED_MESSAGE.into(),
                            kind: "forkOutdated".into(),
                        };
                        return LaunchResult {
                            base_tree,
                            session: None,
                            refused: Some(refused),
                        };
                    }
                    Err(e) => {
                        let message =
                            format!("could not check the thread this fork was made from: {e}");
                        return LaunchResult {
                            base_tree,
                            session: Some(Err(AdapterError::Other(message))),
                            refused: None,
                        };
                    }
                }
            }
            let session = match (permit, adapter) {
                (Some(_), _) if abort.load(Ordering::SeqCst) => None,
                // Awaited to the end even when the turn is cancelled meanwhile: the process
                // exists as soon as the adapter spawned it.
                (Some(permit), Some(adapter)) => {
                    Some(adapter.start(req).await.map(|h| (h, permit)))
                }
                (Some(_), None) => Some(Err(AdapterError::Unavailable(
                    "harness is not configured".into(),
                ))),
                (None, _) => None,
            };
            LaunchResult {
                base_tree,
                session,
                refused: None,
            }
        })
    }

    async fn on_launch_step(&mut self, step: LaunchStep) {
        match step {
            LaunchStep::Permit(permit) => {
                let Some(launch) = self.launch.take() else {
                    return;
                };
                let settings = self.row.settings.clone();
                let fut = self.launching_future(
                    Some(permit),
                    None,
                    None,
                    settings.clone(),
                    launch.abort.clone(),
                );
                self.launch = Some(Launch {
                    phase: Phase::Launching(fut),
                    settings,
                    ..launch
                });
                self.commit_logged(Uow::default()).await;
            }
            LaunchStep::Done(result) => {
                let Some(launch) = self.launch.take() else {
                    return;
                };
                self.on_launched(launch.turn, launch.settings, result).await;
            }
        }
    }

    async fn on_launched(
        &mut self,
        launched_for: TurnId,
        settings: ThreadSettings,
        result: LaunchResult,
    ) {
        let mut uow = Uow::default();
        let mut start_error = result.refused.map(StartFailure::Refused);
        // No new session: the turn goes to the process that was already running.
        let reused = result.session.is_none();
        match result.session {
            Some(Err(e)) => {
                if matches!(e, AdapterError::Unavailable(_)) {
                    // The adapter found its CLI unusable although the last probe did not:
                    // probe again, so that clients (`harness/updated`) and later requests see
                    // it, and the retry schedule takes over.
                    drop(self.sh.probe_harness(
                        self.row.harness_id.clone(),
                        Some(Instant::now()),
                        Publish::IfChanged,
                    ));
                }
                start_error = Some(StartFailure::Adapter(e));
            }
            Some(Ok((handle, permit))) => {
                if let Some(native) = handle.native_session_id.clone() {
                    self.row.native_session_id = Some(native);
                }
                self.row.fork_source = None;
                self.live = Some(Live {
                    control: handle.control,
                    events: handle.events,
                    permit,
                    settings,
                });
                uow.thread_changed = true;
            }
            None => {}
        }
        // The thread is being stopped: stop what was just started; the stop completes once it
        // has exited.
        if let Some(reason) = self.stopping.as_ref().map(|s| s.reason) {
            if let Some(e) = &start_error {
                tracing::info!(thread = %self.row.id, error = %e, "the start that was stopped meanwhile failed");
            }
            if self.live.is_some() {
                self.stop_live_process(reason);
                self.commit_logged(uow).await;
            } else {
                self.settle_stop(uow, Vec::new()).await;
            }
            return;
        }
        let Some(turn) = self.turn.as_ref().filter(|t| !t.sent) else {
            // The turn ended while launching (e.g. interrupted); a started process stays ready.
            if let Some(e) = start_error {
                tracing::warn!(thread = %self.row.id, error = %e, "starting the agent failed after its turn had ended");
            }
            self.commit_logged(uow).await;
            return;
        };
        if turn.id != launched_for {
            // Launched for a turn that ended meanwhile: this turn needs its own snapshot (and a
            // process, if that start failed or was skipped).
            self.commit_logged(uow).await;
            self.begin_launch(None).await;
            return;
        }
        if let Some(failure) = start_error {
            let error = failure.turn_error();
            self.row.last_error = Some(ThreadError {
                message: error.message.clone(),
                kind: error.kind.clone(),
                at: now_ms(),
            });
            self.finish_turn(
                TurnStatus::Failed,
                None,
                Some(error),
                ExpireReason::TurnEnded,
                &mut uow,
            );
            let after = self.after_turn_end(&mut uow).await;
            self.commit_logged(uow).await;
            self.run_after(after).await;
            return;
        }
        // Settings changed since the process was started (during the start, or while an
        // earlier turn ran) reach it before the input does: applied now, or the process is
        // replaced so that the turn runs with them.
        self.apply_pending_settings().await;
        if self.restart_pending && self.live.is_some() {
            self.commit_logged(uow).await;
            self.begin_launch(result.base_tree).await;
            return;
        }
        let turn = self.turn.as_mut().expect("unsent turn");
        turn.base_tree = result.base_tree.clone();
        if self.row.base_tree.is_none() && result.base_tree.is_some() {
            self.row.base_tree = result.base_tree.clone();
            self.row.diff_available = true;
            uow.thread_changed = true;
        }
        let input = turn.input.clone();
        let turn_row = self.turn_row_of(
            self.turn.as_ref().expect("unsent turn"),
            TurnStatus::Running,
        );
        uow.turn_updates.push(turn_row);
        let control = self.live.as_ref().map(|l| l.control.clone());
        let deadline = self.request_deadline();
        let send_result = match control {
            Some(c) => bounded(deadline, "the new turn", c.send(input)).await,
            None => Err(AdapterError::Closed),
        };
        match send_result {
            Ok(()) => {
                if let Some(t) = self.turn.as_mut() {
                    t.sent = true;
                }
                self.commit_logged(uow).await;
            }
            Err(AdapterError::Closed) if reused && self.live.is_some() => {
                // The running process is ending (its `Exited` follows, as every session ends
                // with one): the turn stays unsent and starts on a new process then.
                tracing::info!(thread = %self.row.id, "the agent process ended before the input was sent; the turn waits for a new process");
                self.commit_logged(uow).await;
            }
            Err(AdapterError::TurnInProgress) if self.live.is_some() => {
                // The agent started a run by itself and did not take the input; its
                // `TurnStarted` is already on its way (the adapter emits it first). That run
                // becomes a turn of its own and this one follows it (the deferral in
                // `apply_event`), instead of failing.
                tracing::info!(thread = %self.row.id, "the agent is running a turn of its own; the user's turn waits for it");
                self.commit_logged(uow).await;
            }
            Err(e) => {
                let message = e.to_string();
                self.finish_turn(
                    TurnStatus::Failed,
                    None,
                    Some(TurnError {
                        message,
                        kind: "adapterError".into(),
                    }),
                    ExpireReason::TurnEnded,
                    &mut uow,
                );
                let after = self.after_turn_end(&mut uow).await;
                self.commit_logged(uow).await;
                self.run_after(after).await;
            }
        }
    }

    /// Brings the live process to the thread's settings before a turn is sent to it (see
    /// [`Live::settings`]). A process that cannot apply them live, or fails to, is replaced
    /// (`restart_pending`).
    async fn apply_pending_settings(&mut self) {
        if self.restart_pending {
            return;
        }
        let Some(live) = self
            .live
            .as_ref()
            .filter(|l| l.settings != self.row.settings)
        else {
            return;
        };
        let (control, target) = (live.control.clone(), self.row.settings.clone());
        let deadline = self.request_deadline();
        match bounded(deadline, "the settings", control.apply_settings(&target)).await {
            Ok(SettingsApplied::Live) => {
                if let Some(live) = self.live.as_mut() {
                    live.settings = target;
                }
            }
            Ok(SettingsApplied::RequiresRestart) => self.restart_pending = true,
            Err(e) => {
                tracing::warn!(thread = %self.row.id, error = %e, "applying the thread's settings failed; restarting the process");
                self.restart_pending = true;
            }
        }
    }

    // ----- adapter events ----------------------------------------------------------------------

    async fn on_events(&mut self, first: Option<AdapterEvent>) {
        let Some(first) = first else {
            // The adapter dropped its channel without `Exited` (contract violation): treat it
            // as an exit with unknown status so nothing is left hanging.
            tracing::error!(thread = %self.row.id, "adapter event channel closed without Exited");
            self.on_exited(ExitInfo {
                code: None,
                stopped: None,
                stderr_tail: String::new(),
                exited_at_ms: now_ms(),
            })
            .await;
            return;
        };
        let mut batch = vec![first];
        if let Some(live) = self.live.as_mut() {
            while batch.len() < self.sh.config.policy.max_batch_events {
                match live.events.try_recv() {
                    Ok(ev) => batch.push(ev),
                    Err(_) => break,
                }
            }
        }
        let mut uow = Uow::default();
        let mut exited = None;
        let mut after = Vec::new();
        for ev in batch {
            if let AdapterEvent::Exited { info } = ev {
                exited = Some(info);
                break;
            }
            self.apply_event(ev, &mut uow, &mut after);
        }
        after.extend(self.after_turn_end(&mut uow).await);
        self.commit_logged(uow).await;
        self.run_after(after).await;
        if let Some(info) = exited {
            self.on_exited(info).await;
        } else {
            self.resume_after_background().await;
        }
    }

    fn apply_event(&mut self, ev: AdapterEvent, uow: &mut Uow, after: &mut Vec<After>) {
        match ev {
            AdapterEvent::SessionIdentified { native_session_id } => {
                if self.row.native_session_id.as_deref() != Some(native_session_id.as_str()) {
                    self.row.native_session_id = Some(native_session_id);
                    uow.thread_changed = true;
                }
            }
            AdapterEvent::SessionInfo { model, .. } => {
                if model.is_some() {
                    self.reported_model = model;
                }
            }
            AdapterEvent::CommandsChanged { commands } => {
                self.commands = Some(commands);
                uow.events.push(Event::CommandsChanged {});
            }
            AdapterEvent::SessionTitle { title } => {
                let title =
                    aas_harness::harness_title(&title, self.sh.config.policy.harness_title_chars);
                let replaceable = matches!(
                    self.row.title_source.as_str(),
                    "default" | "firstMessage" | "harness"
                );
                if !title.is_empty() && replaceable && self.row.title != title {
                    self.row.title = title;
                    self.row.title_source = "harness".into();
                    uow.thread_changed = true;
                }
            }
            AdapterEvent::HarnessInfoChanged => after.push(After::Reprobe),
            AdapterEvent::TurnStarted => match &self.turn {
                None if self.live.is_some() => {
                    // The agent started a run by itself: record it as a turn without input.
                    self.open_agent_turn(uow);
                }
                Some(turn) if !turn.sent && self.deferred.is_none() && self.live.is_some() => {
                    // The agent started a run by itself before the user's input went out: the
                    // run gets its own turn and the user's turn follows it (sending while the
                    // agent runs is not allowed, and the run's output is not the answer).
                    if self
                        .launch
                        .as_ref()
                        .is_some_and(|l| l.kind == LaunchKind::Reuse)
                    {
                        self.launch = None;
                    }
                    self.deferred = self.turn.take();
                    self.open_agent_turn(uow);
                    tracing::info!(thread = %self.row.id, "the user's turn waits for the agent's own run");
                }
                _ => {}
            },
            AdapterEvent::ItemStarted { key, body } => self.item_started(key, body, uow),
            AdapterEvent::ItemDelta { key, field, text } => {
                self.item_delta(&key, field, &text, uow)
            }
            AdapterEvent::ItemUpdated { key, body } => self.item_updated(&key, body, uow),
            AdapterEvent::ItemCompleted { key, body, status } => {
                self.item_completed(&key, body, status, uow)
            }
            AdapterEvent::InteractionRequested {
                request_id,
                request,
                item_key,
                background_key,
            } => self.interaction_requested(request_id, request, item_key, background_key, uow),
            AdapterEvent::InteractionWithdrawn { request_id } => {
                // The agent no longer needs the answer, whatever the request belongs to.
                if let Some(p) = self.pending.remove(&request_id) {
                    uow.expire.push((p.id, ExpireReason::HarnessCancelled));
                    uow.thread_changed = true;
                }
            }
            AdapterEvent::BackgroundTask { task } => self.background_task(*task, uow),
            AdapterEvent::TurnUsage { usage } => {
                // Recorded with the running turn and relayed as it comes, so the client sees
                // the context occupancy grow during a long turn.
                let changed = match self.turn.as_mut().filter(|t| t.sent) {
                    Some(turn) if turn.usage != Some(usage) => {
                        turn.usage = Some(usage);
                        true
                    }
                    _ => false,
                };
                if changed {
                    let turn = self.turn.as_ref().expect("the sent turn just updated");
                    uow.events.push(Event::TurnUsageUpdated {
                        turn_id: turn.id.clone(),
                        usage,
                    });
                    uow.turn_updates
                        .push(self.turn_row_of(turn, TurnStatus::Running));
                }
            }
            AdapterEvent::TurnCompleted {
                status,
                usage,
                error,
                trigger,
            } => {
                // A turn whose input was not sent yet cannot be the one that completed.
                let Some(turn) = self.turn.as_mut().filter(|t| t.sent) else {
                    tracing::warn!(thread = %self.row.id, "TurnCompleted without a running turn; ignored");
                    return;
                };
                turn.trigger = trigger;
                // When we asked the process to stop, an interruption reported by the agent
                // records why (the stop request is a known fact, not an inference).
                let error = error.or_else(|| match (&self.stopping, status) {
                    (Some(s), TurnStatus::Interrupted) => Some(stop_error(s.reason, &self.sh)),
                    _ => None,
                });
                self.finish_turn(status, usage, error, ExpireReason::TurnEnded, uow);
            }
            AdapterEvent::Notice {
                level,
                message,
                code,
            } => {
                if self.turn.as_ref().is_some_and(|t| t.sent) {
                    let key = format!("__notice_{}", ulid::Ulid::generate());
                    self.item_started(
                        key.clone(),
                        ItemBody::Notice {
                            level,
                            message,
                            code,
                        },
                        uow,
                    );
                    self.item_completed(&key, None, ItemStatus::Completed, uow);
                } else {
                    uow.events.push(Event::Native {
                        harness_id: self.row.harness_id.clone(),
                        payload: serde_json::json!({ "notice": { "level": level, "message": message, "code": code } }),
                    });
                }
            }
            AdapterEvent::Native { payload } => {
                uow.events.push(Event::Native {
                    harness_id: self.row.harness_id.clone(),
                    payload,
                });
            }
            AdapterEvent::Exited { .. } => unreachable!("handled by on_events"),
        }
    }

    fn item_started(&mut self, key: String, body: ItemBody, uow: &mut Uow) {
        let Some(turn) = self.turn.as_mut().filter(|t| t.sent) else {
            tracing::warn!(thread = %self.row.id, key, "item outside a turn; ignored");
            return;
        };
        if turn.items.contains_key(&key) {
            tracing::warn!(thread = %self.row.id, key, "duplicate item key; treated as an update");
            self.item_updated(&key, body, uow);
            return;
        }
        let now = now_ms();
        let item = Item {
            id: ItemId::generate(),
            thread_id: self.row.id.clone(),
            turn_id: turn.id.clone(),
            status: ItemStatus::InProgress,
            started_at: now,
            completed_at: None,
            background_task_id: None,
            body,
        };
        uow.items_insert.push(item.clone());
        uow.events.push(Event::ItemStarted { item: item.clone() });
        turn.order.push(key.clone());
        turn.keys.insert(key.clone(), item.id.clone());
        turn.items.insert(
            key.clone(),
            OpenItem {
                key,
                item,
                spill: None,
                truncated: false,
            },
        );
    }

    fn item_delta(&mut self, key: &str, field: DeltaField, text: &str, uow: &mut Uow) {
        let max_inline = self.sh.config.policy.max_inline_output_bytes;
        let blobs = self.sh.blobs.clone();
        let Some(open) = self
            .turn
            .as_mut()
            .filter(|t| t.sent)
            .and_then(|t| t.items.get_mut(key))
        else {
            tracing::warn!(thread = %self.row.id, key, "delta for an unknown item; ignored");
            return;
        };
        let is_output = field == DeltaField::Output;
        if !is_output {
            if open.item.body.append(field, text) {
                uow.events.push(Event::ItemDelta {
                    item_id: open.item.id.clone(),
                    field,
                    text: text.to_owned(),
                });
                uow.update_item(open.item.clone());
            }
            return;
        }
        if open.truncated {
            if let Some(spill) = open.spill.as_mut()
                && let Err(e) = spill.write(text.as_bytes())
            {
                tracing::error!(error = %e, "writing spilled output failed");
                open.spill = None;
            }
            return;
        }
        let current = output_len(&open.item.body);
        if current + text.len() <= max_inline {
            if open.item.body.append(field, text) {
                uow.events.push(Event::ItemDelta {
                    item_id: open.item.id.clone(),
                    field,
                    text: text.to_owned(),
                });
                uow.update_item(open.item.clone());
            }
            return;
        }
        // Overflow: keep what fits inline, spill the full output to a blob.
        let room = max_inline.saturating_sub(current);
        let cut = floor_char_boundary(text, room);
        let (head, _) = text.split_at(cut);
        if !head.is_empty() {
            open.item.body.append(field, head);
            uow.events.push(Event::ItemDelta {
                item_id: open.item.id.clone(),
                field,
                text: head.to_owned(),
            });
        }
        let full_so_far = format!("{}{}", output_text(&open.item.body), &text[cut..]);
        match blobs.spill() {
            Ok(mut spill) => {
                if spill.write(full_so_far.as_bytes()).is_ok() {
                    open.spill = Some(spill);
                }
            }
            Err(e) => {
                tracing::error!(error = %e, "creating a spill file failed; further output is dropped")
            }
        }
        open.truncated = true;
        set_truncated(&mut open.item.body, true, None);
        uow.update_item(open.item.clone());
        uow.events.push(Event::ItemUpdated {
            item: open.item.clone(),
        });
    }

    fn item_updated(&mut self, key: &str, body: ItemBody, uow: &mut Uow) {
        let max_inline = self.sh.config.policy.max_inline_output_bytes;
        let blobs = self.sh.blobs.clone();
        let Some(open) = self
            .turn
            .as_mut()
            .filter(|t| t.sent)
            .and_then(|t| t.items.get_mut(key))
        else {
            tracing::warn!(thread = %self.row.id, key, "update for an unknown item; ignored");
            return;
        };
        // The update replaces the whole body, output included: the same size policy as for
        // deltas and completion applies (inline prefix, full text in a spill).
        open.item.body = body;
        if output_len(&open.item.body) > max_inline {
            let full = output_text(&open.item.body);
            open.spill = match blobs.spill() {
                Ok(mut spill) => match spill.write(full.as_bytes()) {
                    Ok(()) => Some(spill),
                    Err(e) => {
                        tracing::error!(error = %e, "writing spilled output failed");
                        None
                    }
                },
                Err(e) => {
                    tracing::error!(error = %e, "creating a spill file failed");
                    None
                }
            };
            let cut = floor_char_boundary(&full, max_inline);
            set_output(&mut open.item.body, &full[..cut]);
            open.truncated = true;
            set_truncated(&mut open.item.body, true, None);
        } else if open.truncated {
            // Rewritten to something that fits inline: the spilled text is obsolete.
            open.spill = None;
            open.truncated = false;
            set_truncated(&mut open.item.body, false, None);
        }
        uow.update_item(open.item.clone());
        uow.events.push(Event::ItemUpdated {
            item: open.item.clone(),
        });
    }

    fn item_completed(
        &mut self,
        key: &str,
        body: Option<ItemBody>,
        status: ItemStatus,
        uow: &mut Uow,
    ) {
        let Some(turn) = self.turn.as_mut().filter(|t| t.sent) else {
            return;
        };
        let Some(open) = turn.items.remove(key) else {
            tracing::warn!(thread = %self.row.id, key, "completion for an unknown item; ignored");
            return;
        };
        turn.order.retain(|k| k != key);
        let item = self.finalize_item(open, body, status, uow);
        uow.update_item(item.clone());
        uow.events.push(Event::ItemCompleted { item });
    }

    /// Closes an item: applies the final body, moves long output to a blob.
    fn finalize_item(
        &self,
        mut open: OpenItem,
        body: Option<ItemBody>,
        status: ItemStatus,
        uow: &mut Uow,
    ) -> Item {
        let max_inline = self.sh.config.policy.max_inline_output_bytes;
        // The background task this item launched, if one names it as its origin.
        if open.item.background_task_id.is_none() {
            open.item.background_task_id = self.background.launched_by(&open.key);
        }
        if status == ItemStatus::Backgrounded && open.item.background_task_id.is_none() {
            tracing::warn!(thread = %self.row.id, key = %open.key, "an item was closed as backgrounded before any background task named it");
        }
        if let Some(mut final_body) = body {
            if open.truncated {
                // Our spill has the full output; keep our inline prefix.
                let inline = output_text(&open.item.body);
                set_output(&mut final_body, &inline);
            } else if output_len(&final_body) > max_inline {
                let full = output_text(&final_body);
                match self.sh.blobs.spill() {
                    Ok(mut spill) => {
                        if spill.write(full.as_bytes()).is_ok() {
                            open.spill = Some(spill);
                        }
                    }
                    Err(e) => tracing::error!(error = %e, "creating a spill file failed"),
                }
                let cut = floor_char_boundary(&full, max_inline);
                set_output(&mut final_body, &full[..cut]);
                open.truncated = true;
            }
            open.item.body = final_body;
        }
        if open.truncated {
            let blob = open
                .spill
                .take()
                .and_then(|spill| match spill.finish(&self.sh.blobs) {
                    Ok((id, size, pin)) => {
                        uow.blobs
                            .push((id.clone(), "text/plain; charset=utf-8".into(), size));
                        uow.pins.push(pin);
                        Some(id)
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "storing spilled output failed");
                        None
                    }
                });
            set_truncated(&mut open.item.body, true, blob);
        }
        open.item.status = status;
        open.item.completed_at = Some(now_ms());
        open.item
    }

    /// Records a request of the agent. It belongs to the background task that asks (named by
    /// the adapter), else to the running turn, else to the thread; it is never dropped, and it
    /// expires with what it belongs to (design.md §8).
    fn interaction_requested(
        &mut self,
        request_id: String,
        request: InteractionRequest,
        item_key: Option<String>,
        background_key: Option<String>,
        uow: &mut Uow,
    ) {
        let task = background_key
            .as_deref()
            .and_then(|key| self.background.get(key))
            .filter(|t| !t.ended())
            .map(|t| t.view.id.clone());
        if let (Some(key), None) = (&background_key, &task) {
            tracing::warn!(thread = %self.row.id, request_id, key, "a request names a background task that is not running; it belongs to the thread");
        }
        let sent_turn = self.turn.as_ref().filter(|t| t.sent);
        let (scope, turn_id, item_id) = match (&task, sent_turn) {
            (Some(task), _) => (Scope::Task(task.clone()), None, None),
            (None, Some(turn)) if background_key.is_none() => (
                Scope::Turn(turn.id.clone()),
                Some(turn.id.clone()),
                item_key.as_ref().and_then(|k| turn.keys.get(k)).cloned(),
            ),
            _ => (Scope::Thread, None, None),
        };
        let anchor_turn_id = match scope {
            Scope::Turn(_) => None,
            Scope::Task(_) | Scope::Thread => self.anchor_turn(),
        };
        let interaction = Interaction {
            id: InteractionId::generate(),
            thread_id: self.row.id.clone(),
            turn_id,
            item_id,
            background_task_id: task,
            status: InteractionStatus::Pending,
            created_at: now_ms(),
            resolved_at: None,
            resolved_by: None,
            request,
            resolution: None,
            expire_reason: None,
        };
        self.pending.insert(
            request_id.clone(),
            PendingInteraction {
                id: interaction.id.clone(),
                scope,
            },
        );
        uow.interactions_insert.push(InteractionRow {
            interaction: interaction.clone(),
            adapter_request_id: request_id,
            anchor_turn_id,
        });
        uow.events.push(Event::InteractionRequested {
            interaction: interaction.clone(),
        });
        uow.ws_events
            .push(Event::InteractionPending { interaction });
        uow.thread_changed = true;
        self.row.last_activity_at = now_ms();
    }

    fn turn_row_of(&self, t: &ActiveTurn, status: TurnStatus) -> TurnRow {
        TurnRow {
            turn: Turn {
                id: t.id.clone(),
                thread_id: self.row.id.clone(),
                index: t.index,
                status,
                started_at: t.started_at,
                completed_at: None,
                model: self
                    .reported_model
                    .clone()
                    .or_else(|| self.row.settings.model.clone()),
                error: None,
                usage: t.usage,
                diff: None,
                trigger: t.trigger,
            },
            base_tree: t.base_tree.clone(),
            end_tree: None,
        }
    }

    /// Ends the active turn: closes open items, expires pending interactions, updates the
    /// turn and thread rows.
    fn finish_turn(
        &mut self,
        status: TurnStatus,
        usage: Option<Usage>,
        error: Option<TurnError>,
        expire_reason: ExpireReason,
        uow: &mut Uow,
    ) {
        let Some(turn) = self.turn.take() else { return };
        self.interrupt_deadline = None;
        self.close_turn(turn, status, usage, error, expire_reason, uow);
    }

    /// Ends `turn` (the active or a deferred one).
    fn close_turn(
        &mut self,
        mut turn: ActiveTurn,
        status: TurnStatus,
        usage: Option<Usage>,
        error: Option<TurnError>,
        expire_reason: ExpireReason,
        uow: &mut Uow,
    ) {
        let mut row = self.turn_row_of(&turn, status);
        let item_status = match status {
            TurnStatus::Completed | TurnStatus::Running => ItemStatus::Completed,
            TurnStatus::Interrupted => ItemStatus::Interrupted,
            TurnStatus::Failed => ItemStatus::Failed,
        };
        for key in std::mem::take(&mut turn.order) {
            if let Some(open) = turn.items.remove(&key) {
                let item = self.finalize_item(open, None, item_status, uow);
                uow.update_item(item.clone());
                uow.events.push(Event::ItemCompleted { item });
            }
        }
        let scope = Scope::Turn(turn.id.clone());
        self.expire_pending(|s| *s == scope, expire_reason, uow);
        let now = now_ms();
        // A final usage without the context occupancy keeps the one the turn last reported.
        let usage = match (usage, turn.usage) {
            (Some(mut last), Some(running)) if last.context.is_none() => {
                last.context = running.context;
                Some(last)
            }
            (last, running) => last.or(running),
        };
        row.turn.completed_at = Some(now);
        row.turn.usage = usage;
        row.turn.error = error;
        uow.events.push(Event::TurnCompleted {
            turn: row.turn.clone(),
        });
        uow.turn_updates.push(row);
        if let Some(u) = &usage {
            self.row.usage.accumulate(u);
        }
        self.row.last_activity_at = now;
        if status != TurnStatus::Completed && self.queue_len > 0 {
            self.row.queue_paused = true;
        }
        uow.thread_changed = true;
        self.sh.turn_finished();
        drop(turn);
    }

    /// Follow-up work after turns ended in `uow` (diff summary, the next turn). The end
    /// snapshot is taken now and stored with the turn, so a client that sees `turn/completed`
    /// can diff the turn right away.
    async fn after_turn_end(&mut self, uow: &mut Uow) -> Vec<After> {
        let needs_snapshot = uow
            .turn_updates
            .iter()
            .any(|r| r.turn.status.is_terminal() && r.base_tree.is_some());
        let head = if needs_snapshot {
            self.end_snapshot().await
        } else {
            None
        };
        let mut after = Vec::new();
        for row in &mut uow.turn_updates {
            if row.turn.status.is_terminal() {
                if let (Some(base), Some(head)) = (&row.base_tree, &head) {
                    row.end_tree = Some(head.clone());
                    after.push(After::DiffSummary {
                        turn: row.turn.id.clone(),
                        base: base.clone(),
                        head: head.clone(),
                    });
                }
                if self.deferred.is_some() {
                    after.push(After::ResumeDeferred);
                } else if row.turn.status == TurnStatus::Completed
                    && self.queue_len > 0
                    && !self.row.queue_paused
                    && self.stopping.is_none()
                {
                    after.push(After::StartNextQueued);
                }
            }
        }
        after
    }

    /// Snapshot of the working tree at the end of a turn (kept reachable for the thread).
    fn end_snapshot(&self) -> impl Future<Output = Option<String>> + Send + 'static {
        let git = self.sh.git.clone();
        let (cwd, thread) = (PathBuf::from(&self.row.cwd), self.row.id.to_string());
        async move {
            match git?.snapshot_tree(&cwd, Some(&thread)).await {
                Ok(tree) => tree,
                Err(e) => {
                    tracing::warn!(thread = %thread, error = %e, "git snapshot at turn end failed");
                    None
                }
            }
        }
    }

    async fn run_after(&mut self, after: Vec<After>) {
        // End snapshot of a turn that ended in this batch: the base of a turn that starts right
        // after it (so no change falls between the two). Never kept beyond this pass: a turn
        // started later takes its own snapshot.
        let mut end_tree: Option<String> = None;
        for a in after {
            match a {
                After::DiffSummary { turn, base, head } => {
                    self.spawn_diff_summary(turn, base, head.clone());
                    end_tree = Some(head);
                }
                After::Reprobe => {
                    // Runs by itself; clients learn the result from `harness/updated`.
                    drop(self.sh.probe_harness(
                        self.row.harness_id.clone(),
                        Some(Instant::now()),
                        Publish::IfChanged,
                    ));
                }
                After::StartNextQueued => {
                    if self.turn.is_some() || !self.sh.accepts_work() || self.stopping.is_some() {
                        continue;
                    }
                    match self.pop_queued().await {
                        Ok(Some(next)) => match self.convert_input(&next.input).await {
                            Ok(c) => {
                                let mut uow = Uow {
                                    queue_changed: true,
                                    pins: c.pins,
                                    ..Default::default()
                                };
                                uow.queued_delete.push(next.id);
                                self.open_turn(
                                    c.input,
                                    c.text,
                                    c.attachments,
                                    c.mentions,
                                    &mut uow,
                                );
                                self.commit_logged(uow).await;
                                self.begin_launch(end_tree.take()).await;
                            }
                            Err(CoreError::Rpc(e)) => {
                                // The entry itself cannot be sent (e.g. its image is gone):
                                // it leaves the queue, and the queue goes on.
                                tracing::warn!(thread = %self.row.id, error = %e, "queued input is invalid; dropped");
                                let mut uow = Uow {
                                    queue_changed: true,
                                    thread_changed: true,
                                    ..Default::default()
                                };
                                uow.queued_delete.push(next.id);
                                self.commit_logged(uow).await;
                            }
                            Err(e) => {
                                // Not the entry's fault: it stays, and the queue waits for the
                                // user instead of going on without it.
                                tracing::error!(thread = %self.row.id, error = %e, "the next queued input could not be read; the queue is paused");
                                self.queue_len += 1;
                                self.row.queue_paused = true;
                                self.commit_logged(Uow {
                                    thread_changed: true,
                                    ..Default::default()
                                })
                                .await;
                            }
                        },
                        Ok(None) => {}
                        Err(e) => {
                            tracing::error!(thread = %self.row.id, error = %e, "reading the queue failed; the queue is paused");
                            self.row.queue_paused = true;
                            self.commit_logged(Uow {
                                thread_changed: true,
                                ..Default::default()
                            })
                            .await;
                        }
                    }
                }
                After::ResumeDeferred => {
                    if self.turn.is_some() {
                        continue;
                    }
                    if let Some(turn) = self.deferred.take() {
                        self.turn = Some(turn);
                        self.begin_launch(end_tree.take()).await;
                    }
                }
            }
        }
    }

    fn spawn_diff_summary(&self, turn: TurnId, base: String, head: String) {
        let Some(git) = self.sh.git.clone() else {
            return;
        };
        let sh = self.sh.clone();
        let cwd = PathBuf::from(&self.row.cwd);
        let thread_id = self.row.id.clone();
        tokio::spawn(async move {
            let files = match git.diff_files(&cwd, &base, &head).await {
                Ok(f) => f,
                Err(e) => {
                    tracing::warn!(thread = %thread_id, error = %e, "git diff failed");
                    return;
                }
            };
            let diff = DiffSummary {
                files: files.len() as u32,
                insertions: files.iter().map(|f| f.added).sum(),
                deletions: files.iter().map(|f| f.removed).sum(),
            };
            let result = sh
                .tx_durable("turn diff summary", move |tx, em| {
                    if let Some(mut row) = store::get_turn(tx, &turn)? {
                        row.turn.diff = Some(diff);
                        store::update_turn(tx, &row)?;
                        em.thread(
                            &thread_id,
                            Event::TurnDiffUpdated {
                                turn_id: turn.clone(),
                                diff,
                            },
                        );
                    }
                    Ok(())
                })
                .await;
            if let Err(e) = result {
                tracing::error!(error = %e, "storing the diff summary failed");
            }
        });
    }

    async fn on_exited(&mut self, info: ExitInfo) {
        tracing::info!(thread = %self.row.id, outcome = %info.describe(), "agent process ended");
        self.live = None;
        // A snapshot-only launch was for the process that just ended.
        if self
            .launch
            .as_ref()
            .is_some_and(|l| l.kind == LaunchKind::Reuse)
        {
            self.launch = None;
        }
        let stop_reason = self.stopping.as_ref().map(|s| s.reason);
        let mut uow = Uow::default();
        if self.turn.as_ref().is_some_and(|t| t.sent) {
            let forced = self.turn.as_ref().is_some_and(|t| t.forced);
            let (status, error) = match (stop_reason, forced) {
                (_, true) => (
                    TurnStatus::Interrupted,
                    TurnError {
                        message: "the agent did not stop in time and was terminated".into(),
                        kind: "forced".into(),
                    },
                ),
                (Some(reason), false) => (TurnStatus::Interrupted, stop_error(reason, &self.sh)),
                (None, false) => (
                    TurnStatus::Failed,
                    TurnError {
                        message: exit_message(
                            &info,
                            self.sh.config.policy.exit_message_stderr_lines,
                        ),
                        kind: "agentExited".into(),
                    },
                ),
            };
            self.finish_turn(
                status,
                None,
                Some(error),
                ExpireReason::ProcessExited,
                &mut uow,
            );
        }
        // A user turn that waited behind the agent's own run never reached this process: it
        // runs on a new one (`settle_stop`).
        if self.turn.is_none() {
            self.turn = self.deferred.take();
        }
        // Background work ends with the process: stopped when we stopped it, lost otherwise.
        let end = match stop_reason {
            Some(reason) => ProcessEnd::Stopped(background_end_reason(reason, &self.sh)),
            None => ProcessEnd::Lost,
        };
        let lost = self.end_background(end, &mut uow);
        if stop_reason.is_none() && (!info.is_clean() || lost > 0) {
            let mut message = exit_message(&info, self.sh.config.policy.exit_message_stderr_lines);
            if lost > 0 {
                message.push_str(&format!(
                    " ({lost} background task{} lost)",
                    if lost == 1 { " was" } else { "s were" }
                ));
            }
            self.row.last_error = Some(ThreadError {
                message,
                kind: "agentExited".into(),
                at: now_ms(),
            });
        }
        let after = self.after_turn_end(&mut uow).await;
        self.settle_stop(uow, after).await;
    }

    /// The process is gone (or none was started): completes a pending stop (answers
    /// `thread/stop`, continues `thread/archive`), runs the follow-up work, and starts a turn
    /// that is still waiting for a process.
    async fn settle_stop(&mut self, mut uow: Uow, after: Vec<After>) {
        let stopping = self.stopping.take();
        uow.thread_changed = true;
        let mut archive_requests = Vec::new();
        if let Some(s) = stopping {
            for (idem, reply) in s.waiters {
                if let Some(idem) = idem {
                    uow.idem_thread.push(idem);
                }
                // Replies are sent after the commit below.
                archive_requests.push((None, reply));
            }
            for (req, idem, reply) in s.archive {
                archive_requests.push((Some((req, idem)), reply));
            }
        }
        let view = self.commit(uow).await;
        for (archive, reply) in archive_requests {
            match archive {
                None => {
                    let r = match &view {
                        Ok(Some(v)) => Ok(ThreadResult { thread: v.clone() }),
                        Ok(None) => self
                            .current_view()
                            .await
                            .map(|thread| ThreadResult { thread }),
                        Err(e) => Err(CoreError::Internal(e.to_string())),
                    };
                    send_reply(reply, r);
                }
                Some((req, idem)) => {
                    let r = self.apply_archive(req, idem).await;
                    send_reply(reply, r);
                }
            }
        }
        if let Err(e) = view {
            tracing::error!(thread = %self.row.id, error = %e, "failed to persist process exit");
        }
        self.run_after(after).await;
        self.launch_pending_turn().await;
    }

    /// A turn sent while the process was being stopped (or waiting behind the agent's own run)
    /// starts now, on a new process.
    async fn launch_pending_turn(&mut self) {
        if !self.turn.as_ref().is_some_and(|t| !t.sent)
            || self.launch.is_some()
            || self.live.is_some()
        {
            return;
        }
        let refusal = if self.row.archived {
            Some(TurnError {
                message: "thread archived".into(),
                kind: "stopped".into(),
            })
        } else if self.shutting_down {
            Some(shutdown_error(&self.sh))
        } else {
            None
        };
        match refusal {
            Some(error) => {
                let mut uow = Uow::default();
                self.finish_turn(
                    TurnStatus::Interrupted,
                    None,
                    Some(error),
                    ExpireReason::TurnEnded,
                    &mut uow,
                );
                self.commit_logged(uow).await;
            }
            None => self.begin_launch(None).await,
        }
    }
}

impl Actor {
    // ----- background tasks and pending interactions ---------------------------------------------

    /// The turn a background task or an interaction that belongs to no turn is shown with: the
    /// running turn, else the thread's latest turn.
    fn anchor_turn(&self) -> Option<TurnId> {
        self.turn
            .as_ref()
            .filter(|t| t.sent)
            .map(|t| t.id.clone())
            .or_else(|| self.last_turn.clone())
    }

    /// Applies the adapter's report of one background task (its whole state; the same state
    /// twice changes nothing).
    fn background_task(&mut self, info: BackgroundTaskInfo, uow: &mut Uow) {
        if self
            .background
            .get(&info.key)
            .is_some_and(|known| known.info == info)
        {
            return;
        }
        let now = now_ms();
        let status = background::status_of(info.state);
        let ended = status.is_terminal();
        let runs = info.runs.max(1);
        let prev = self.background.get(&info.key).map(|t| t.view.clone());
        // Whether the report continues the run clients know. A new run starts when `runs` goes
        // up, or when a task that ended runs again.
        let same_run = prev
            .as_ref()
            .is_some_and(|v| v.runs == runs && !(v.status.is_terminal() && !ended));
        let prev_ended = same_run && prev.as_ref().is_some_and(|v| v.status.is_terminal());
        let started_at = match &prev {
            Some(v) if same_run => v.started_at,
            _ => now,
        };
        let ended_at = match &prev {
            _ if !ended => None,
            Some(v) if prev_ended => v.ended_at.or(Some(now)),
            _ => Some(now),
        };
        let result = self.background_result(&info, uow);
        let origin_item_id = prev
            .as_ref()
            .and_then(|v| v.origin_item_id.clone())
            .or_else(|| {
                let key = info.origin_item_key.as_ref()?;
                self.turn.as_ref()?.keys.get(key).cloned()
            });
        let parent_task_id = prev
            .as_ref()
            .and_then(|v| v.parent_task_id.clone())
            .or_else(|| {
                let key = info.parent_key.as_deref()?;
                self.background.get(key).map(|t| t.view.id.clone())
            });
        if let (Some(key), None) = (&info.origin_item_key, &origin_item_id) {
            tracing::debug!(thread = %self.row.id, task = %info.key, item = %key, "the item that launched a background task is not an item of the running turn");
        }
        let view = BackgroundTask {
            id: prev
                .as_ref()
                .map(|v| v.id.clone())
                .unwrap_or_else(BackgroundTaskId::generate),
            thread_id: self.row.id.clone(),
            native_id: info.key.clone(),
            kind: info.kind,
            title: info.title.clone(),
            status,
            ambient: info.ambient,
            runs,
            turn_id: prev
                .as_ref()
                .and_then(|v| v.turn_id.clone())
                .or_else(|| self.anchor_turn()),
            origin_item_id,
            parent_task_id,
            started_at,
            ended_at,
            end_reason: ended.then_some(BackgroundEndReason::Harness),
            progress: info.progress.clone(),
            result,
            usage: info.usage,
            stoppable: info.stoppable,
            stop_requested_at: prev
                .as_ref()
                .filter(|_| same_run && !ended)
                .and_then(|v| v.stop_requested_at),
            stop_unconfirmed_at: prev
                .as_ref()
                .filter(|_| same_run && !ended)
                .and_then(|v| v.stop_unconfirmed_at),
            next_run_at: info.next_run_at,
        };
        if background::changes_summary(prev.as_ref(), &view) {
            uow.thread_changed = true;
        }
        let key = info.key.clone();
        let task_id = view.id.clone();
        let interval = self.sh.config.policy.background_progress_interval;
        match self.background.get_mut(&key) {
            Some(tracked) => {
                let at = Instant::now();
                // Progress of a running task is written at most once per interval; the latest
                // state waits (`on_background_deadline`), and any other change writes it.
                let wait = tracked
                    .written
                    .as_ref()
                    .is_some_and(|w| background::progress_only(w, &view))
                    && tracked.written_at.is_some_and(|w| at < w + interval);
                tracked.info = info;
                tracked.view = view;
                if ended || !same_run {
                    tracked.stop_deadline = None;
                }
                if !wait {
                    self.write_task(&key, uow);
                } else if tracked.written.as_ref() != Some(&tracked.view) {
                    tracked.flush_at = tracked
                        .flush_at
                        .or_else(|| tracked.written_at.map(|w| w + interval));
                }
            }
            None => {
                self.background.insert(Tracked {
                    info,
                    view,
                    written: None,
                    written_at: None,
                    flush_at: None,
                    stop_deadline: None,
                });
                self.write_task(&key, uow);
            }
        }
        if ended && !prev_ended {
            let scope = Scope::Task(task_id);
            self.expire_pending(|s| *s == scope, ExpireReason::TaskEnded, uow);
        }
    }

    /// What a task produced, as clients see it: an output longer than
    /// `policy.max_inline_output_bytes` is kept whole in a blob (written once per output).
    fn background_result(
        &self,
        info: &BackgroundTaskInfo,
        uow: &mut Uow,
    ) -> Option<BackgroundResult> {
        let reported = info.result.as_ref()?;
        if let Some(known) = self.background.get(&info.key)
            && let Some(prev) = &known.view.result
            && known.info.result.as_ref().map(|r| &r.output) == Some(&reported.output)
        {
            return Some(BackgroundResult {
                summary: reported.summary.clone(),
                exit_code: reported.exit_code,
                ..prev.clone()
            });
        }
        let max_inline = self.sh.config.policy.max_inline_output_bytes;
        let (output, output_truncated, output_blob_id) = match &reported.output {
            None => (None, false, None),
            Some(text) if text.len() <= max_inline => (Some(text.clone()), false, None),
            Some(text) => {
                let blob = self.spill_text(text, uow);
                let cut = floor_char_boundary(text, max_inline);
                (Some(text[..cut].to_owned()), true, blob)
            }
        };
        Some(BackgroundResult {
            summary: reported.summary.clone(),
            exit_code: reported.exit_code,
            output,
            output_truncated,
            output_blob_id,
        })
    }

    /// Stores `text` as a blob referenced by this commit.
    fn spill_text(&self, text: &str, uow: &mut Uow) -> Option<BlobId> {
        let mut spill = match self.sh.blobs.spill() {
            Ok(spill) => spill,
            Err(e) => {
                tracing::error!(error = %e, "creating a spill file failed; the output is kept cut");
                return None;
            }
        };
        if let Err(e) = spill.write(text.as_bytes()) {
            tracing::error!(error = %e, "writing spilled output failed; the output is kept cut");
            return None;
        }
        match spill.finish(&self.sh.blobs) {
            Ok((id, size, pin)) => {
                uow.blobs
                    .push((id.clone(), "text/plain; charset=utf-8".into(), size));
                uow.pins.push(pin);
                Some(id)
            }
            Err(e) => {
                tracing::error!(error = %e, "storing spilled output failed; the output is kept cut");
                None
            }
        }
    }

    /// Writes the current state of task `key` (when it differs from what clients know).
    fn write_task(&mut self, key: &str, uow: &mut Uow) {
        let Some(tracked) = self.background.get_mut(key) else {
            return;
        };
        tracked.flush_at = None;
        if tracked.written.as_ref() == Some(&tracked.view) {
            return;
        }
        tracked.written = Some(tracked.view.clone());
        tracked.written_at = Some(Instant::now());
        uow.background.push(tracked.view.clone());
        uow.events.push(Event::BackgroundTaskUpdated {
            task: tracked.view.clone(),
        });
    }

    /// A coalesced progress update is due, or a stop request went unconfirmed.
    async fn on_background_deadline(&mut self) {
        let now = Instant::now();
        let mut uow = Uow::default();
        for key in self.background.due(now) {
            if let Some(tracked) = self.background.get_mut(&key)
                && tracked.stop_deadline.is_some_and(|d| d <= now)
            {
                tracked.stop_deadline = None;
                if !tracked.ended() {
                    // Nothing is escalated: the task goes on, and the user may ask again or stop
                    // the thread.
                    tracing::warn!(thread = %self.row.id, task = %tracked.view.id, "the harness did not confirm the stop of a background task in time");
                    tracked.view.stop_requested_at = None;
                    tracked.view.stop_unconfirmed_at = Some(now_ms());
                }
            }
            self.write_task(&key, &mut uow);
        }
        self.commit_logged(uow).await;
    }

    /// Ends every background task of the process that has not ended (the process ends, or is
    /// replaced), and expires what waited for an answer from it outside a turn. Returns how
    /// many tasks that were not ambient were lost.
    fn end_background(&mut self, end: ProcessEnd, uow: &mut Uow) -> usize {
        let now = now_ms();
        let mut lost = 0;
        for key in self.background.unfinished() {
            let Some(tracked) = self.background.get_mut(&key) else {
                continue;
            };
            let (status, reason) = match end {
                ProcessEnd::Stopped(reason) => (BackgroundTaskStatus::Stopped, reason),
                ProcessEnd::Lost => {
                    if !tracked.view.ambient {
                        lost += 1;
                    }
                    (
                        BackgroundTaskStatus::Lost,
                        BackgroundEndReason::ProcessExited,
                    )
                }
            };
            tracked.info.live = false;
            tracked.view.status = status;
            tracked.view.ended_at = Some(now);
            tracked.view.end_reason = Some(reason);
            tracked.view.stop_requested_at = None;
            tracked.stop_deadline = None;
            self.write_task(&key, uow);
            uow.thread_changed = true;
        }
        self.expire_pending(
            |s| !matches!(s, Scope::Turn(_)),
            ExpireReason::ProcessExited,
            uow,
        );
        self.background.clear();
        lost
    }

    /// Expires the pending interactions whose scope matches. When the process lives and the
    /// request expired because what it belonged to ended, the adapter answers the agent after
    /// the commit (`Uow::answers`), so that it does not wait for an answer that never comes.
    fn expire_pending(
        &mut self,
        matches: impl Fn(&Scope) -> bool,
        reason: ExpireReason,
        uow: &mut Uow,
    ) {
        let answer = self.live.is_some()
            && matches!(reason, ExpireReason::TurnEnded | ExpireReason::TaskEnded);
        let expired: Vec<String> = self
            .pending
            .iter()
            .filter(|(_, p)| matches(&p.scope))
            .map(|(request_id, _)| request_id.clone())
            .collect();
        for request_id in expired {
            if let Some(p) = self.pending.remove(&request_id) {
                uow.expire.push((p.id, reason));
                uow.thread_changed = true;
                if answer {
                    uow.answers.push((request_id, reason));
                }
            }
        }
    }

    /// `backgroundTask/stop`: asks the harness to stop task `task_id`. The request only asks;
    /// the end arrives from the harness (design.md §5.6).
    async fn stop_background(
        &mut self,
        task_id: BackgroundTaskId,
        idem: Option<Idem>,
    ) -> CoreResult<BackgroundTaskResult> {
        let Some(key) = self.background.key_of(&task_id).map(str::to_owned) else {
            // Not a task of the running process: one that ended with an earlier process, or
            // none of this thread.
            let (id, thread) = (task_id.clone(), self.row.id.clone());
            let stored = self
                .sh
                .db
                .read(move |tx| store::get_background_task(tx, &id))
                .await?;
            return Err(match stored {
                Some(t) if t.thread_id == thread => {
                    invalid_state("the background task is not running")
                }
                _ => not_found("backgroundTask", &task_id),
            });
        };
        let tracked = self
            .background
            .get(&key)
            .expect("the key of a tracked task");
        if tracked.ended() {
            return Err(invalid_state("the background task is not running"));
        }
        if let Some(info) = self.sh.registry.info(&self.row.harness_id)
            && info.available
            && !info.capabilities.background_stop
        {
            return Err(rpc(
                ErrorKind::CapabilityUnsupported,
                "this harness cannot stop single background tasks",
            )
            .with("capability", "backgroundStop"));
        }
        if !tracked.info.stoppable {
            return Err(invalid_state(
                "the harness cannot stop this background task on its own; stop the thread to stop everything",
            ));
        }
        let control = self
            .live
            .as_ref()
            .map(|l| l.control.clone())
            .ok_or_else(|| invalid_state("the agent is not running"))?;
        let deadline = self.request_deadline();
        bounded(deadline, "the stop request", control.stop_background(&key))
            .await
            .map_err(adapter_err(&self.row.harness_id))?;
        let confirm = self.sh.config.policy.background_stop_confirm_timeout;
        let mut uow = Uow::default();
        let tracked = self
            .background
            .get_mut(&key)
            .expect("the key of a tracked task");
        tracked.view.stop_requested_at = Some(now_ms());
        tracked.view.stop_unconfirmed_at = None;
        tracked.stop_deadline = Some(Instant::now() + confirm);
        let task = tracked.view.clone();
        self.write_task(&key, &mut uow);
        let result = BackgroundTaskResult { task };
        if let Some(idem) = idem {
            uow.idem_values.push((idem, serde_json::to_value(&result)?));
        }
        self.commit(uow).await?;
        Ok(result)
    }
}

/// How background work ends with its process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProcessEnd {
    /// We stopped the process, for this reason.
    Stopped(BackgroundEndReason),
    /// The process ended by itself: how its work ended is unknown.
    Lost,
}

/// Why background work ended when we stopped its process for `reason`.
fn background_end_reason(reason: StopReason, sh: &Shared) -> BackgroundEndReason {
    match reason {
        StopReason::User | StopReason::Abandoned => BackgroundEndReason::ThreadStopped,
        StopReason::Idle => BackgroundEndReason::IdleStop,
        StopReason::InterruptTimeout => BackgroundEndReason::ForcedStop,
        StopReason::Shutdown => {
            if sh.session_ending.load(std::sync::atomic::Ordering::SeqCst) {
                BackgroundEndReason::SystemShutdown
            } else {
                BackgroundEndReason::DaemonShutdown
            }
        }
    }
}

/// Error recorded on a turn the daemon's shutdown ended: `systemShutdown` when Windows ends
/// the session, else `daemonShutdown`.
fn shutdown_error(sh: &Shared) -> TurnError {
    if sh.session_ending.load(std::sync::atomic::Ordering::SeqCst) {
        TurnError {
            message: crate::engine::SYSTEM_SHUTDOWN_MESSAGE.into(),
            kind: "systemShutdown".into(),
        }
    } else {
        TurnError {
            message: "the daemon is shutting down".into(),
            kind: "daemonShutdown".into(),
        }
    }
}

/// Error recorded on a turn that ended because we stopped its process.
fn stop_error(reason: StopReason, sh: &Shared) -> TurnError {
    match reason {
        StopReason::Shutdown => shutdown_error(sh),
        StopReason::InterruptTimeout => TurnError {
            message: "the agent did not stop in time and was terminated".into(),
            kind: "forced".into(),
        },
        StopReason::Idle => TurnError {
            message: "the agent was stopped while idle".into(),
            kind: "stopped".into(),
        },
        StopReason::User | StopReason::Abandoned => TurnError {
            message: "stopped by the user".into(),
            kind: "stopped".into(),
        },
    }
}

/// What a turn's error says about an unexpected exit, quoting the last `stderr_lines` lines of
/// the agent's stderr (`policy.exit_message_stderr_lines`).
fn exit_message(info: &ExitInfo, stderr_lines: usize) -> String {
    let tail = info.stderr_tail.trim();
    let last_lines: Vec<&str> = tail
        .lines()
        .rev()
        .take(stderr_lines)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    if last_lines.is_empty() {
        format!("the agent {}", info.describe())
    } else {
        format!("the agent {}: {}", info.describe(), last_lines.join("\n"))
    }
}

/// A thread title from the first non-empty line of `text`, at most `max_chars` characters
/// (`policy.first_message_title_chars`). The same rule adapters apply to native sessions
/// without a name ([`aas_harness::title_from_first_line`]).
fn title_from(text: &str, max_chars: usize) -> String {
    aas_harness::title_from_first_line(text, max_chars)
}

fn adapter_err(harness: &str) -> impl Fn(AdapterError) -> CoreError + '_ {
    move |e| match e {
        AdapterError::Unsupported(cap) => rpc(
            ErrorKind::CapabilityUnsupported,
            format!("not supported by {harness}: {cap}"),
        )
        .with("capability", cap),
        AdapterError::Unavailable(r) => rpc(ErrorKind::HarnessUnavailable, r),
        other => rpc(ErrorKind::AdapterError, other.to_string())
            .with("harnessId", harness)
            .with("detail", other.to_string()),
    }
}

fn floor_char_boundary(s: &str, index: usize) -> usize {
    if index >= s.len() {
        return s.len();
    }
    let mut i = index;
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn output_text(body: &ItemBody) -> String {
    match body {
        ItemBody::CommandExecution { output, .. } => output.clone(),
        ItemBody::ToolCall { output, .. } => output.clone().unwrap_or_default(),
        _ => String::new(),
    }
}

fn output_len(body: &ItemBody) -> usize {
    match body {
        ItemBody::CommandExecution { output, .. } => output.len(),
        ItemBody::ToolCall { output, .. } => output.as_ref().map_or(0, String::len),
        _ => 0,
    }
}

fn set_output(body: &mut ItemBody, text: &str) {
    match body {
        ItemBody::CommandExecution { output, .. } => *output = text.to_owned(),
        ItemBody::ToolCall { output, .. } => *output = Some(text.to_owned()),
        _ => {}
    }
}

fn set_truncated(body: &mut ItemBody, truncated: bool, blob: Option<BlobId>) {
    match body {
        ItemBody::CommandExecution {
            output_truncated,
            output_blob_id,
            ..
        }
        | ItemBody::ToolCall {
            output_truncated,
            output_blob_id,
            ..
        } => {
            *output_truncated = truncated;
            if blob.is_some() {
                *output_blob_id = blob;
            }
        }
        _ => {}
    }
}

/// Checks that a resolution answers its request.
pub fn validate_resolution(
    request: &InteractionRequest,
    resolution: &InteractionResolution,
) -> CoreResult<()> {
    match (request, resolution) {
        (_, InteractionResolution::Dismissed) => Ok(()),
        (
            InteractionRequest::Approval { options, .. },
            InteractionResolution::Approval { option_id, .. },
        ) => {
            if options.iter().any(|o| &o.id == option_id) {
                Ok(())
            } else {
                Err(invalid_params(format!("unknown option {option_id}")))
            }
        }
        (
            InteractionRequest::Question { questions, .. },
            InteractionResolution::Question { answers },
        ) => {
            for answer in answers {
                let Some(q) = questions.iter().find(|q| q.id == answer.question_id) else {
                    return Err(invalid_params(format!(
                        "unknown question {}",
                        answer.question_id
                    )));
                };
                if !q.multi_select && answer.choice_ids.len() > 1 {
                    return Err(invalid_params(format!(
                        "question {} accepts one choice",
                        q.id
                    )));
                }
                if let Some(bad) = answer
                    .choice_ids
                    .iter()
                    .find(|c| !q.choices.iter().any(|ch| &ch.id == *c))
                {
                    return Err(invalid_params(format!("unknown choice {bad}")));
                }
                if answer.text.is_some() && !q.allow_free_text {
                    return Err(invalid_params(format!(
                        "question {} does not accept free text",
                        q.id
                    )));
                }
            }
            Ok(())
        }
        _ => Err(invalid_params(
            "the resolution does not match the request kind",
        )),
    }
}

/// Checks settings against the harness' advertised models, effort levels and modes.
pub fn validate_settings(info: &aas_harness::HarnessInfo, s: &ThreadSettings) -> CoreResult<()> {
    if let Some(m) = &s.model
        && !info.models.is_empty()
        && !info.models.iter().any(|x| &x.id == m)
    {
        return Err(invalid_params(format!("unknown model {m}")));
    }
    if let Some(e) = &s.effort
        && !info.effort_levels.iter().any(|x| &x.id == e)
    {
        return Err(invalid_params(format!("unknown effort level {e}")));
    }
    if let Some(p) = &s.permission_mode
        && !info.permission_modes.is_empty()
        && !info.permission_modes.iter().any(|x| &x.id == p)
    {
        return Err(invalid_params(format!("unknown permission mode {p}")));
    }
    Ok(())
}

trait CoreErrorWith {
    fn with(self, key: &str, value: impl Into<Value>) -> Self;
}

impl CoreErrorWith for CoreError {
    fn with(self, key: &str, value: impl Into<Value>) -> Self {
        match self {
            CoreError::Rpc(e) => CoreError::Rpc(e.with(key, value)),
            other => other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_and_exit_messages_follow_their_policy_lengths() {
        assert_eq!(title_from("\n  first line  \nsecond", 80), "first line");
        assert_eq!(title_from("abcdef", 3), "abc…");
        assert_eq!(title_from("abc", 3), "abc");
        let info = ExitInfo {
            code: Some(1),
            stopped: None,
            stderr_tail: "one\ntwo\nthree\n".into(),
            exited_at_ms: 0,
        };
        assert_eq!(
            exit_message(&info, 2),
            "the agent exited with code 1: two\nthree"
        );
        assert_eq!(exit_message(&info, 0), "the agent exited with code 1");
    }
}
