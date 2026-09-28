//! The fake agent: executes deterministic scenarios written in the prompt.
//!
//! A prompt is a list of lines. Lines starting with `@` are directives; other lines are
//! collected into one agent message. A prompt without directives is answered with
//! `echo: <prompt>`.
//!
//! | directive | effect |
//! |---|---|
//! | `@text <words…>` | agent message streamed in chunks |
//! | `@reason <words…>` | reasoning item |
//! | `@stream <n> [interval_ms]` | agent message made of `n` deltas (`tok0 tok1 …`) |
//! | `@exec <command…>` | command execution without approval |
//! | `@approve <command…>` | command execution that asks for approval first |
//! | `@question` | asks a question, then reports the answer |
//! | `@plan` | plan item that progresses through its entries |
//! | `@write <relpath> <content…>` | writes the file under the cwd and reports a file change |
//! | `@sleep <ms>` | waits (interruptible) |
//! | `@fail <message…>` | ends the turn as failed |
//! | `@crash [code]` | the agent process exits immediately (default code 3) |
//! | `@withdraw` | asks for approval and withdraws the request by itself |
//! | `@bigoutput <bytes>` | command execution streaming `bytes` of output in 1 KiB deltas |
//! | `@context <used> <window>` | reports the turn's usage so far with a context-window occupancy of `used` of `window` tokens (also carried by the final usage) |
//! | `@hang [ms]` | keeps the turn running for `ms` (default 60 s) without reacting to interrupts or to its input ending: only terminating the process ends it early |
//! | `@bg <key> [options…] [title…]` | starts a background task that goes on after the turn (see below) |
//!
//! Attached images are acknowledged with an extra message `received <n> image(s) (<bytes> bytes)`.
//!
//! # Background tasks (`@bg`)
//!
//! `@bg <key> [options…] [title…]` starts background task `key` (see [`crate::background`]).
//! The turn reports a launching item (a command for `kind=shell`, a tool call otherwise), the
//! task (running, in the live set), and closes the item as `backgrounded`; the task then runs
//! on its own, across turns, until it ends by itself or is stopped. Options (before the title;
//! the first other word starts the title, default `<kind> <key>`):
//!
//! | option | effect |
//! |---|---|
//! | `kind=<k>` | `agent` (default), `shell`, `workflow`, `monitor`, `remote`, `scheduled`, `other` |
//! | `ms=<n>` | each run lasts `n` ms (default 1000); `0`: until it is stopped |
//! | `end=completed\|failed` | how a run ends by itself (default `completed`) |
//! | `exit=<code>` | exit code of the result (shell tasks report 0, or 1 when failed, without it) |
//! | `progress=<n>` | progress reports per run (a workflow reports its agents) |
//! | `parent=<key>` | launched by that background task |
//! | `restart=<n>` | starts again under the same key `n` times after a run ended (`runs`) |
//! | `ambient` | the harness marks it as not being activity |
//! | `wake` | when it ends, the agent starts a turn by itself about it (trigger `backgroundTask`; `scheduled` for `kind=scheduled`) |
//! | `approve` | asks for approval (belonging to the task) during its first run; a denial fails it |
//! | `unstoppable` | cannot be stopped on its own |
//! | `stubborn` | ignores stop requests (it still dies with the agent) |
//! | `detached` | no launching item |
//!
//! A turn the agent starts by itself runs after the current turn; a prompt that arrives while
//! it runs is refused with `promptAck { accepted: false, ownRun: true }` (the adapter then
//! reports [`aas_harness::AdapterError::TurnInProgress`]).
//!
//! # Sessions
//!
//! When the adapter names a session store in its `hello` (`sessionsDir`), the agent keeps its
//! sessions there like a real CLI (see [`crate::store`]): a new session creates its transcript,
//! every finished turn is appended to it, a resumed session must exist, and a fork
//! (`forkFrom`) creates a new session holding the source's turns. A session that cannot be
//! opened is answered with `rejected` (and the agent exits with [`REJECTED_EXIT_CODE`]), like a
//! CLI whose `--resume` names no session. Without a store nothing is written, a resume is taken
//! as given and a fork is rejected.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use aas_protocol::types::*;
use aas_stdio::{JsonLinesReader, JsonLinesWriter, ReadLine};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{Mutex, mpsc, watch};

use crate::background::{BackgroundGuard, BackgroundRuntime, BackgroundSpec, Wake};
use crate::store::{RecordedItem, RecordedTurn, SessionStore};
use crate::wire::{Ev, Op};

/// Characters per delta when streaming text, unless the options say otherwise.
pub const DEFAULT_CHUNK_SIZE: usize = 4;
/// Upper bound of one line the agent reads from its input (ops are small; a prompt with a
/// large pasted text still fits).
const MAX_OP_LINE_BYTES: usize = 16 * 1024 * 1024;
/// Scenario defaults of directives given without their numbers.
const DEFAULT_STREAM_COUNT: u32 = 10;
const DEFAULT_SLEEP_MS: u64 = 1000;
const DEFAULT_HANG_MS: u64 = 60_000;
const DEFAULT_CRASH_CODE: i32 = 3;
const DEFAULT_BIG_OUTPUT_BYTES: usize = 100_000;
/// Bytes per delta of `@bigoutput`.
const BIG_OUTPUT_CHUNK_BYTES: usize = 1024;
/// Bytes of one generated `@bigoutput` line (`line 00000042\n`).
const BIG_OUTPUT_LINE_BYTES: usize = 14;
/// Item keys each turn may use (`k<n>`): turn `t` numbers its items from `t * ITEM_KEYS_PER_TURN`,
/// so keys never repeat within a session.
const ITEM_KEYS_PER_TURN: u64 = 1000;
/// Exit code of an agent whose `hello` was rejected (its session could not be opened).
pub const REJECTED_EXIT_CODE: i32 = 1;
/// How long a turn the agent starts by itself runs after its message, like a short model
/// answer: a prompt sent right after that turn started finds it running (and is refused).
const OWN_RUN_MS: u64 = 200;

/// Options of the fake agent.
#[derive(Debug, Clone)]
pub struct AgentOptions {
    pub cwd: PathBuf,
    /// Characters per delta when streaming text.
    pub chunk_size: usize,
}

impl Default for AgentOptions {
    fn default() -> Self {
        Self {
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            chunk_size: DEFAULT_CHUNK_SIZE,
        }
    }
}

/// One step of a scenario.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    Text(String),
    Reason(String),
    Stream {
        count: u32,
        interval_ms: u64,
    },
    Exec(String),
    Approve(String),
    Question,
    Plan,
    Write {
        path: String,
        content: String,
    },
    Sleep(u64),
    Fail(String),
    Crash(i32),
    Withdraw,
    BigOutput(usize),
    Context {
        used: u64,
        window: u64,
    },
    Hang(u64),
    Background(BackgroundSpec),
    Unknown(String),
    /// A directive whose arguments are wrong (reported as a warning notice).
    Invalid(String),
}

/// Parses a prompt into scenario steps.
pub fn parse_script(prompt: &str) -> Vec<Step> {
    let mut steps = Vec::new();
    let mut text = String::new();
    let mut saw_directive = false;
    let flush = |text: &mut String, steps: &mut Vec<Step>| {
        let t = text.trim();
        if !t.is_empty() {
            steps.push(Step::Text(t.to_owned()));
        }
        text.clear();
    };
    for line in prompt.lines() {
        let trimmed = line.trim();
        let Some(rest) = trimmed.strip_prefix('@') else {
            if !trimmed.is_empty() {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(trimmed);
            }
            continue;
        };
        let (name, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
        let args = args.trim();
        // Mentions such as "@src/main.rs" are not directives.
        let known = matches!(
            name,
            "text"
                | "reason"
                | "stream"
                | "exec"
                | "approve"
                | "question"
                | "plan"
                | "write"
                | "sleep"
                | "fail"
                | "crash"
                | "withdraw"
                | "bigoutput"
                | "context"
                | "hang"
                | "bg"
        );
        if !known {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(trimmed);
            continue;
        }
        saw_directive = true;
        flush(&mut text, &mut steps);
        let mut nums = args.split_whitespace();
        steps.push(match name {
            "text" => Step::Text(args.to_owned()),
            "reason" => Step::Reason(args.to_owned()),
            "stream" => Step::Stream {
                count: nums
                    .next()
                    .and_then(|n| n.parse().ok())
                    .unwrap_or(DEFAULT_STREAM_COUNT),
                interval_ms: nums.next().and_then(|n| n.parse().ok()).unwrap_or(0),
            },
            "exec" => Step::Exec(args.to_owned()),
            "approve" => Step::Approve(if args.is_empty() {
                "echo hi".to_owned()
            } else {
                args.to_owned()
            }),
            "question" => Step::Question,
            "plan" => Step::Plan,
            "write" => {
                let (path, content) = args.split_once(char::is_whitespace).unwrap_or((args, ""));
                Step::Write {
                    path: path.to_owned(),
                    content: content.to_owned(),
                }
            }
            "sleep" => Step::Sleep(
                nums.next()
                    .and_then(|n| n.parse().ok())
                    .unwrap_or(DEFAULT_SLEEP_MS),
            ),
            "fail" => Step::Fail(if args.is_empty() {
                "failed on purpose".to_owned()
            } else {
                args.to_owned()
            }),
            "crash" => Step::Crash(
                nums.next()
                    .and_then(|n| n.parse().ok())
                    .unwrap_or(DEFAULT_CRASH_CODE),
            ),
            "withdraw" => Step::Withdraw,
            "bigoutput" => Step::BigOutput(
                nums.next()
                    .and_then(|n| n.parse().ok())
                    .unwrap_or(DEFAULT_BIG_OUTPUT_BYTES),
            ),
            "hang" => Step::Hang(
                nums.next()
                    .and_then(|n| n.parse().ok())
                    .unwrap_or(DEFAULT_HANG_MS),
            ),
            "context" => Step::Context {
                used: nums.next().and_then(|n| n.parse().ok()).unwrap_or(0),
                window: nums.next().and_then(|n| n.parse().ok()).unwrap_or(0),
            },
            "bg" => match BackgroundSpec::parse(args) {
                Ok(spec) => Step::Background(spec),
                Err(message) => Step::Invalid(message),
            },
            other => Step::Unknown(other.to_owned()),
        });
    }
    flush(&mut text, &mut steps);
    if !saw_directive {
        let echo = format!("echo: {}", prompt.trim());
        return vec![Step::Text(echo)];
    }
    steps
}

/// Where the agent's events go: to the adapter and, during a turn of a stored session, into
/// the turn's record as well.
pub(crate) struct Emitter<W> {
    writer: Arc<Mutex<JsonLinesWriter<W>>>,
    record: Option<Arc<parking_lot::Mutex<TurnRecord>>>,
}

impl<W> Emitter<W> {
    /// The same output without a turn's record (background work is not part of a turn).
    pub(crate) fn detached(&self) -> Self {
        Self {
            writer: self.writer.clone(),
            record: None,
        }
    }
}

impl<W> Clone for Emitter<W> {
    fn clone(&self) -> Self {
        Self {
            writer: self.writer.clone(),
            record: self.record.clone(),
        }
    }
}

pub(crate) async fn emit<W: AsyncWrite + Unpin>(w: &Emitter<W>, ev: Ev) -> bool {
    if let Some(record) = &w.record {
        record.lock().observe(&ev);
    }
    w.writer.lock().await.send(&ev).await.is_ok()
}

/// What a turn produced, kept for the session transcript: the items as the adapter sees them
/// (started, streamed, updated, completed), in order.
pub(crate) struct TurnRecord {
    started_at: Millis,
    items: Vec<(Option<String>, RecordedItem)>,
}

impl TurnRecord {
    fn new(prompt: &str) -> Self {
        let mut record = Self::own();
        record.user_message(prompt, UserMessageDelivery::Normal);
        record
    }

    /// A turn the agent started by itself (no user message).
    fn own() -> Self {
        Self {
            started_at: now_ms(),
            items: Vec::new(),
        }
    }

    fn user_message(&mut self, text: &str, delivery: UserMessageDelivery) {
        self.items.push((
            None,
            RecordedItem {
                body: ItemBody::UserMessage {
                    text: text.to_owned(),
                    attachments: Vec::new(),
                    mentions: Vec::new(),
                    delivery,
                },
                status: ItemStatus::Completed,
            },
        ));
    }

    fn notice(&mut self, level: NoticeLevel, message: &str, code: Option<&str>) {
        self.items.push((
            None,
            RecordedItem {
                body: ItemBody::Notice {
                    level,
                    message: message.to_owned(),
                    code: code.map(str::to_owned),
                },
                status: ItemStatus::Completed,
            },
        ));
    }

    fn item(&mut self, key: &str) -> Option<&mut RecordedItem> {
        self.items
            .iter_mut()
            .rev()
            .find(|(k, _)| k.as_deref() == Some(key))
            .map(|(_, item)| item)
    }

    fn observe(&mut self, ev: &Ev) {
        match ev {
            Ev::ItemStarted { key, body } => self.items.push((
                Some(key.clone()),
                RecordedItem {
                    body: body.clone(),
                    status: ItemStatus::InProgress,
                },
            )),
            Ev::Delta { key, field, text } => {
                if let Some(item) = self.item(key) {
                    item.body.append(*field, text);
                }
            }
            Ev::ItemUpdated { key, body } => {
                if let Some(item) = self.item(key) {
                    item.body = body.clone();
                }
            }
            Ev::ItemCompleted { key, status, body } => {
                if let Some(item) = self.item(key) {
                    item.status = *status;
                    if let Some(body) = body {
                        item.body = body.clone();
                    }
                }
            }
            Ev::Notice { level, message } => self.notice(*level, message, None),
            Ev::Ready { .. }
            | Ev::Rejected { .. }
            | Ev::SessionInfo { .. }
            | Ev::Commands { .. }
            | Ev::PromptAck { .. }
            | Ev::TurnStarted
            | Ev::Request { .. }
            | Ev::Withdraw { .. }
            | Ev::Usage { .. }
            | Ev::TurnCompleted { .. }
            | Ev::Background { .. } => {}
        }
    }

    fn finish(&self, status: TurnStatus) -> RecordedTurn {
        RecordedTurn {
            started_at: self.started_at,
            completed_at: now_ms(),
            status,
            items: self.items.iter().map(|(_, item)| item.clone()).collect(),
        }
    }
}

fn now_ms() -> Millis {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as Millis)
        .unwrap_or(0)
}

/// The stored session the agent runs.
#[derive(Clone)]
struct Session {
    store: SessionStore,
    id: String,
}

/// Opens the session a `hello` names (see the module docs); the error is the rejection message.
fn open_session(
    cwd: &std::path::Path,
    id: &str,
    resume: bool,
    fork_from: Option<&str>,
    sessions_dir: Option<&str>,
) -> Result<Option<Session>, String> {
    let Some(dir) = sessions_dir else {
        return match fork_from {
            Some(_) => Err("forking needs a session store (harness option `sessionsDir`)".into()),
            None => Ok(None),
        };
    };
    let store = SessionStore::new(dir);
    let opened = match (fork_from, resume) {
        (Some(source), _) => store.fork(source, id, cwd),
        (None, true) => store.read(id).map(|_| ()),
        (None, false) => store.create(id, cwd),
    };
    opened.map_err(|e| e.to_string())?;
    Ok(Some(Session {
        store,
        id: id.to_owned(),
    }))
}

struct TurnChannels {
    interrupt_tx: watch::Sender<bool>,
    respond_tx: mpsc::UnboundedSender<(String, InteractionResolution)>,
    steer_tx: mpsc::UnboundedSender<String>,
}

/// The task running the current turn, aborted when dropped: a turn never outlives its agent
/// (like the threads of a process that is killed), also when the agent itself is aborted.
struct TurnTask(Option<tokio::task::AbortHandle>);

impl TurnTask {
    /// Tracks the task of a new turn (the previous turn has ended: one runs at a time).
    fn track(&mut self, task: tokio::task::AbortHandle) {
        self.0 = Some(task);
    }
}

impl Drop for TurnTask {
    fn drop(&mut self) {
        if let Some(task) = self.0.take() {
            task.abort();
        }
    }
}

/// How a turn ended, as the agent's main loop learns it.
enum TurnEnd {
    /// The turn is over (sent before its completion is reported, see [`TurnCtx::finish`]).
    Done,
    /// The scenario crashes the agent with this exit code.
    Crash(i32),
}

/// Runs the fake agent until its input ends or a scenario crashes it. Returns the exit code.
pub async fn run_agent<R, W>(reader: R, writer: W, options: AgentOptions) -> i32
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let writer = Emitter {
        writer: Arc::new(Mutex::new(JsonLinesWriter::new(writer))),
        record: None,
    };
    let mut reader = JsonLinesReader::new(reader, MAX_OP_LINE_BYTES);
    let mut session: Option<Session> = None;
    let (end_tx, mut end_rx) = mpsc::unbounded_channel::<TurnEnd>();
    let mut turn: Option<TurnChannels> = None;
    let mut turn_task = TurnTask(None);
    // Background work (`@bg`), and the turns it makes the agent start by itself: they run
    // after the current turn, in order. Dropped with the agent, which ends its tasks.
    let (wake_tx, mut wake_rx) = mpsc::unbounded_channel::<Wake>();
    let background = BackgroundRuntime::new(wake_tx);
    let _background_guard = BackgroundGuard(background.clone());
    let mut wakes: std::collections::VecDeque<Wake> = std::collections::VecDeque::new();
    // Whether the running turn is one the agent started by itself.
    let mut own_run = false;
    // Set while a `@hang` step runs: the agent then outlives the end of its input.
    let hanging = Arc::new(AtomicBool::new(false));
    let mut model: Option<String> = Some("fake-fast".into());
    let mut item_counter = 0u64;

    // Starts the next turn of the agent's own when none runs.
    macro_rules! start_own_run {
        () => {
            if turn.is_none()
                && let Some(wake) = wakes.pop_front()
            {
                let (channels, ctx) = turn_ctx(
                    &writer,
                    &session,
                    None,
                    &options,
                    &mut item_counter,
                    &hanging,
                    &end_tx,
                    &background,
                    Some(wake.trigger),
                );
                turn = Some(channels);
                own_run = true;
                let end_tx = end_tx.clone();
                let task = tokio::spawn(async move {
                    let steps = vec![Step::Text(wake.text), Step::Sleep(OWN_RUN_MS)];
                    if let TurnEnd::Crash(code) = run_turn(ctx, steps).await {
                        let _ = end_tx.send(TurnEnd::Crash(code));
                    }
                });
                turn_task.track(task.abort_handle());
            }
        };
    }

    loop {
        tokio::select! {
            end = end_rx.recv() => {
                match end {
                    Some(TurnEnd::Crash(code)) => return code,
                    Some(TurnEnd::Done) => { turn = None; own_run = false; start_own_run!(); }
                    None => {}
                }
            }
            Some(wake) = wake_rx.recv() => {
                wakes.push_back(wake);
                start_own_run!();
            }
            line = reader.next() => {
                let op = match line {
                    Ok(Some(ReadLine::Json(v))) => match serde_json::from_value::<Op>(v) {
                        Ok(op) => op,
                        Err(e) => {
                            emit(&writer, Ev::Notice { level: NoticeLevel::Error, message: format!("bad op: {e}") }).await;
                            continue;
                        }
                    },
                    Ok(Some(ReadLine::NotJson(_))) => continue,
                    // An input that fails ends like one that is closed (a CLI whose stdin
                    // breaks stops reading); the adapter sees the agent's exit.
                    Ok(None) | Err(_) => {
                        if let Some(t) = &turn {
                            let _ = t.interrupt_tx.send(true);
                        }
                        if hanging.load(Ordering::SeqCst) {
                            // Ignores the end of its input like a CLI stuck in a tool: it
                            // goes on until the turn is over or the process is terminated.
                            if let Some(TurnEnd::Crash(code)) = end_rx.recv().await {
                                return code;
                            }
                        }
                        return 0;
                    }
                };
                // A turn that has reported its completion has sent `Done` before: whatever the
                // adapter sends in response to the completion finds the turn over. A turn of the
                // agent's own that is due starts first (like a CLI that takes up a notification
                // before the next input).
                while let Ok(end) = end_rx.try_recv() {
                    match end {
                        TurnEnd::Crash(code) => return code,
                        TurnEnd::Done => {
                            turn = None;
                            own_run = false;
                        }
                    }
                }
                while let Ok(wake) = wake_rx.try_recv() {
                    wakes.push_back(wake);
                }
                start_own_run!();
                match op {
                    Op::Hello { session_id, resume, fork_from, sessions_dir } => {
                        match open_session(&options.cwd, &session_id, resume, fork_from.as_deref(), sessions_dir.as_deref()) {
                            Ok(opened) => session = opened,
                            Err(message) => {
                                emit(&writer, Ev::Rejected { message }).await;
                                return REJECTED_EXIT_CODE;
                            }
                        }
                        emit(&writer, Ev::Ready { session_id }).await;
                        emit(&writer, Ev::SessionInfo { model: model.clone() }).await;
                        emit(&writer, Ev::Commands { commands: fake_commands() }).await;
                    }
                    Op::SetModel { model: m } => {
                        model = m;
                        emit(&writer, Ev::SessionInfo { model: model.clone() }).await;
                    }
                    Op::Prompt { text, images } => {
                        if turn.is_some() {
                            emit(&writer, Ev::PromptAck { accepted: false, own_run }).await;
                            continue;
                        }
                        emit(&writer, Ev::PromptAck { accepted: true, own_run: false }).await;
                        let (channels, ctx) = turn_ctx(
                            &writer,
                            &session,
                            Some(&text),
                            &options,
                            &mut item_counter,
                            &hanging,
                            &end_tx,
                            &background,
                            None,
                        );
                        turn = Some(channels);
                        let end_tx = end_tx.clone();
                        let mut steps = parse_script(&text);
                        if !images.is_empty() {
                            steps.push(Step::Text(describe_images(&images)));
                        }
                        let task = tokio::spawn(async move {
                            if let TurnEnd::Crash(code) = run_turn(ctx, steps).await {
                                // The main loop owns the receiver for the agent's whole life.
                                let _ = end_tx.send(TurnEnd::Crash(code));
                            }
                        });
                        turn_task.track(task.abort_handle());
                    }
                    Op::Steer { text, images } => match &turn {
                        Some(t) => {
                            let text = if images.is_empty() { text } else { format!("{text} ({})", describe_images(&images)) };
                            let _ = t.steer_tx.send(text);
                        }
                        None => { emit(&writer, Ev::Notice { level: NoticeLevel::Warning, message: "nothing to steer".into() }).await; }
                    },
                    Op::Interrupt => {
                        if let Some(t) = &turn {
                            let _ = t.interrupt_tx.send(true);
                        }
                    }
                    Op::Respond { request_id, resolution } => {
                        // A background task's request, else the turn's.
                        let resolution = match background.answer(&request_id, resolution.clone()) {
                            true => continue,
                            false => resolution,
                        };
                        if let Some(t) = &turn {
                            let _ = t.respond_tx.send((request_id, resolution));
                        }
                    }
                    Op::StopBackground { key } => {
                        if !background.stop(&key) {
                            emit(&writer, Ev::Notice { level: NoticeLevel::Warning, message: format!("no running background task {key}") }).await;
                        }
                    }
                }
            }
        }
    }
}

/// How the agent acknowledges attached images: how many, their total size, and how many it
/// could not read.
fn describe_images(paths: &[String]) -> String {
    let mut bytes = 0u64;
    let mut unreadable = 0usize;
    for path in paths {
        match std::fs::metadata(path) {
            Ok(meta) => bytes += meta.len(),
            Err(_) => unreadable += 1,
        }
    }
    let n = paths.len();
    let mut out = format!(
        "received {n} image{} ({bytes} bytes)",
        if n == 1 { "" } else { "s" }
    );
    if unreadable > 0 {
        out.push_str(&format!(", {unreadable} unreadable"));
    }
    out
}

fn fake_commands() -> Vec<Command> {
    vec![Command {
        name: "fake-help".into(),
        description: Some("Show the fake agent's scenario directives".into()),
        source: CommandSource::Harness,
        argument_hint: None,
        action: CommandAction::InsertText {
            text: "/fake-help ".into(),
        },
    }]
}

struct TurnCtx<W> {
    writer: Emitter<W>,
    /// The stored session the turn belongs to (its transcript gets the finished turn).
    session: Option<Session>,
    interrupt: watch::Receiver<bool>,
    responses: mpsc::UnboundedReceiver<(String, InteractionResolution)>,
    steers: mpsc::UnboundedReceiver<String>,
    options: AgentOptions,
    first_item: u64,
    hanging: Arc<AtomicBool>,
    /// Tells the main loop that the turn is over.
    end: mpsc::UnboundedSender<TurnEnd>,
    /// The agent's background work (`@bg` starts tasks there).
    background: Arc<BackgroundRuntime>,
    /// Why the agent started this turn by itself (`None` for a prompt).
    trigger: Option<TurnTrigger>,
}

/// The channels and context of a new turn: for `prompt`, or one the agent starts by itself
/// (`prompt` is `None`, `trigger` says why).
// One call site per kind of turn, each passing the main loop's state as it is.
#[allow(clippy::too_many_arguments)]
fn turn_ctx<W>(
    writer: &Emitter<W>,
    session: &Option<Session>,
    prompt: Option<&str>,
    options: &AgentOptions,
    item_counter: &mut u64,
    hanging: &Arc<AtomicBool>,
    end: &mpsc::UnboundedSender<TurnEnd>,
    background: &Arc<BackgroundRuntime>,
    trigger: Option<TurnTrigger>,
) -> (TurnChannels, TurnCtx<W>) {
    let (interrupt_tx, interrupt_rx) = watch::channel(false);
    let (respond_tx, respond_rx) = mpsc::unbounded_channel();
    let (steer_tx, steer_rx) = mpsc::unbounded_channel();
    let record = session.as_ref().map(|_| {
        Arc::new(parking_lot::Mutex::new(match prompt {
            Some(text) => TurnRecord::new(text),
            None => TurnRecord::own(),
        }))
    });
    let ctx = TurnCtx {
        writer: Emitter {
            writer: writer.writer.clone(),
            record,
        },
        session: session.clone(),
        interrupt: interrupt_rx,
        responses: respond_rx,
        steers: steer_rx,
        options: options.clone(),
        first_item: *item_counter,
        hanging: hanging.clone(),
        end: end.clone(),
        background: background.clone(),
        trigger,
    };
    *item_counter += ITEM_KEYS_PER_TURN;
    (
        TurnChannels {
            interrupt_tx,
            respond_tx,
            steer_tx,
        },
        ctx,
    )
}

impl<W: AsyncWrite + Unpin + Send> TurnCtx<W> {
    fn interrupted(&self) -> bool {
        *self.interrupt.borrow()
    }

    /// Takes in steered input: recorded as the user's message and acknowledged with a notice.
    async fn steered(&self, text: String) {
        if let Some(record) = &self.writer.record {
            record
                .lock()
                .user_message(&text, UserMessageDelivery::Steer);
        }
        emit(
            &self.writer,
            Ev::Notice {
                level: NoticeLevel::Info,
                message: format!("steered: {text}"),
            },
        )
        .await;
    }

    /// Appends the finished turn to the session transcript (when the session is stored). A
    /// transcript that cannot be written is reported to the adapter, within the turn.
    async fn save(&self, status: TurnStatus) {
        let (Some(session), Some(record)) = (&self.session, &self.writer.record) else {
            return;
        };
        let turn = record.lock().finish(status);
        if let Err(e) = session.store.append_turn(&session.id, &turn) {
            emit(
                &self.writer,
                Ev::Notice {
                    level: NoticeLevel::Error,
                    message: format!("the session transcript could not be written: {e}"),
                },
            )
            .await;
        }
    }

    /// Ends the turn: saves it, then reports its completion.
    async fn finish(&self, status: TurnStatus, usage: Option<Usage>, error: Option<TurnError>) {
        if let (Some(record), Some(error)) = (&self.writer.record, &error) {
            record
                .lock()
                .notice(NoticeLevel::Error, &error.message, Some(&error.kind));
        }
        self.save(status).await;
        // Before the completion is reported: a prompt sent in response to it must find the turn
        // over. The main loop owns the receiver for the agent's whole life.
        let _ = self.end.send(TurnEnd::Done);
        emit(
            &self.writer,
            Ev::TurnCompleted {
                status,
                usage,
                error,
                trigger: self.trigger,
            },
        )
        .await;
    }

    /// Sleeps unless interrupted first; drains steer messages meanwhile. Returns `false` when
    /// the turn was interrupted.
    async fn pause(&mut self, ms: u64) -> bool {
        let sleep = tokio::time::sleep(Duration::from_millis(ms));
        tokio::pin!(sleep);
        loop {
            tokio::select! {
                _ = &mut sleep => return !self.interrupted(),
                changed = self.interrupt.changed() => {
                    if changed.is_err() || *self.interrupt.borrow() {
                        return false;
                    }
                }
                Some(text) = self.steers.recv() => self.steered(text).await,
            }
        }
    }

    async fn wait_response(&mut self, request_id: &str) -> Option<InteractionResolution> {
        loop {
            tokio::select! {
                changed = self.interrupt.changed() => {
                    if changed.is_err() || *self.interrupt.borrow() {
                        return None;
                    }
                }
                Some(text) = self.steers.recv() => self.steered(text).await,
                resp = self.responses.recv() => match resp {
                    Some((id, res)) if id == request_id => return Some(res),
                    Some(_) => continue,
                    None => return None,
                }
            }
        }
    }
}

async fn run_turn<W: AsyncWrite + Unpin + Send + 'static>(
    mut ctx: TurnCtx<W>,
    steps: Vec<Step>,
) -> TurnEnd {
    let w = ctx.writer.clone();
    emit(&w, Ev::TurnStarted).await;
    let mut next_key = ctx.first_item;
    let mut key = || {
        next_key += 1;
        format!("k{next_key}")
    };
    let mut output_tokens = 0u64;
    let mut context: Option<ContextUsage> = None;
    let usage_now = |output_tokens: u64, context: Option<ContextUsage>| Usage {
        input_tokens: 10,
        output_tokens,
        cached_input_tokens: 0,
        reasoning_tokens: 0,
        cost_usd: None,
        context,
    };
    let mut open_items: Vec<String> = Vec::new();

    let status = 'steps: {
        for step in steps {
            if ctx.interrupted() {
                break 'steps TurnStatus::Interrupted;
            }
            match step {
                Step::Text(text) | Step::Reason(text) if text.is_empty() => {}
                Step::Text(text) => {
                    let k = key();
                    emit(
                        &w,
                        Ev::ItemStarted {
                            key: k.clone(),
                            body: ItemBody::AgentMessage {
                                text: String::new(),
                            },
                        },
                    )
                    .await;
                    open_items.push(k.clone());
                    let chars: Vec<char> = text.chars().collect();
                    for chunk in chars.chunks(ctx.options.chunk_size.max(1)) {
                        let piece: String = chunk.iter().collect();
                        output_tokens += 1;
                        emit(
                            &w,
                            Ev::Delta {
                                key: k.clone(),
                                field: DeltaField::Text,
                                text: piece,
                            },
                        )
                        .await;
                        if !ctx.pause(0).await {
                            break 'steps TurnStatus::Interrupted;
                        }
                    }
                    emit(
                        &w,
                        Ev::ItemCompleted {
                            key: k.clone(),
                            status: ItemStatus::Completed,
                            body: None,
                        },
                    )
                    .await;
                    open_items.retain(|x| x != &k);
                }
                Step::Reason(text) => {
                    let k = key();
                    emit(
                        &w,
                        Ev::ItemStarted {
                            key: k.clone(),
                            body: ItemBody::Reasoning {
                                text: String::new(),
                            },
                        },
                    )
                    .await;
                    emit(
                        &w,
                        Ev::Delta {
                            key: k.clone(),
                            field: DeltaField::Text,
                            text,
                        },
                    )
                    .await;
                    emit(
                        &w,
                        Ev::ItemCompleted {
                            key: k,
                            status: ItemStatus::Completed,
                            body: None,
                        },
                    )
                    .await;
                }
                Step::Stream { count, interval_ms } => {
                    let k = key();
                    emit(
                        &w,
                        Ev::ItemStarted {
                            key: k.clone(),
                            body: ItemBody::AgentMessage {
                                text: String::new(),
                            },
                        },
                    )
                    .await;
                    open_items.push(k.clone());
                    for i in 0..count {
                        output_tokens += 1;
                        emit(
                            &w,
                            Ev::Delta {
                                key: k.clone(),
                                field: DeltaField::Text,
                                text: format!("tok{i} "),
                            },
                        )
                        .await;
                        if !ctx.pause(interval_ms).await {
                            break 'steps TurnStatus::Interrupted;
                        }
                    }
                    emit(
                        &w,
                        Ev::ItemCompleted {
                            key: k.clone(),
                            status: ItemStatus::Completed,
                            body: None,
                        },
                    )
                    .await;
                    open_items.retain(|x| x != &k);
                }
                Step::Exec(command) => {
                    let k = key();
                    emit(
                        &w,
                        Ev::ItemStarted {
                            key: k.clone(),
                            body: command_body(&command, &ctx.options, "", None),
                        },
                    )
                    .await;
                    emit(
                        &w,
                        Ev::Delta {
                            key: k.clone(),
                            field: DeltaField::Output,
                            text: format!("ran {command}\n"),
                        },
                    )
                    .await;
                    emit(
                        &w,
                        Ev::ItemCompleted {
                            key: k,
                            status: ItemStatus::Completed,
                            body: Some(command_body(
                                &command,
                                &ctx.options,
                                &format!("ran {command}\n"),
                                Some(0),
                            )),
                        },
                    )
                    .await;
                }
                Step::Approve(command) => {
                    let k = key();
                    emit(
                        &w,
                        Ev::ItemStarted {
                            key: k.clone(),
                            body: command_body(&command, &ctx.options, "", None),
                        },
                    )
                    .await;
                    open_items.push(k.clone());
                    let request_id = format!("r{}", k);
                    emit(
                        &w,
                        Ev::Request {
                            request_id: request_id.clone(),
                            request: InteractionRequest::Approval {
                                title: "Run command?".into(),
                                detail: None,
                                subject: Subject::Command {
                                    command: command.clone(),
                                    cwd: Some(ctx.options.cwd.display().to_string()),
                                },
                                options: vec![
                                    ApprovalOption {
                                        id: "allow".into(),
                                        label: "Allow".into(),
                                        kind: ApprovalOptionKind::AllowOnce,
                                    },
                                    ApprovalOption {
                                        id: "allow_session".into(),
                                        label: "Allow for this session".into(),
                                        kind: ApprovalOptionKind::AllowForSession,
                                    },
                                    ApprovalOption {
                                        id: "deny".into(),
                                        label: "Deny".into(),
                                        kind: ApprovalOptionKind::Deny,
                                    },
                                ],
                            },
                            item_key: Some(k.clone()),
                            background_key: None,
                        },
                    )
                    .await;
                    let Some(resolution) = ctx.wait_response(&request_id).await else {
                        break 'steps TurnStatus::Interrupted;
                    };
                    let allowed = matches!(&resolution, InteractionResolution::Approval { option_id, .. } if option_id.starts_with("allow"));
                    if allowed {
                        let out = format!("ran {command}\n");
                        emit(
                            &w,
                            Ev::Delta {
                                key: k.clone(),
                                field: DeltaField::Output,
                                text: out.clone(),
                            },
                        )
                        .await;
                        emit(
                            &w,
                            Ev::ItemCompleted {
                                key: k.clone(),
                                status: ItemStatus::Completed,
                                body: Some(command_body(&command, &ctx.options, &out, Some(0))),
                            },
                        )
                        .await;
                    } else {
                        emit(
                            &w,
                            Ev::ItemCompleted {
                                key: k.clone(),
                                status: ItemStatus::Declined,
                                body: None,
                            },
                        )
                        .await;
                    }
                    open_items.retain(|x| x != &k);
                }
                Step::BigOutput(bytes) => {
                    let k = key();
                    let command = format!("generate {bytes} bytes");
                    emit(
                        &w,
                        Ev::ItemStarted {
                            key: k.clone(),
                            body: command_body(&command, &ctx.options, "", None),
                        },
                    )
                    .await;
                    let mut left = bytes;
                    let mut line = 0usize;
                    while left > 0 {
                        let size = BIG_OUTPUT_CHUNK_BYTES.min(left);
                        let mut chunk = String::with_capacity(size + BIG_OUTPUT_LINE_BYTES);
                        while chunk.len() < size {
                            chunk.push_str(&format!("line {line:08}\n"));
                            line += 1;
                        }
                        chunk.truncate(size);
                        left -= chunk.len();
                        emit(
                            &w,
                            Ev::Delta {
                                key: k.clone(),
                                field: DeltaField::Output,
                                text: chunk,
                            },
                        )
                        .await;
                    }
                    emit(
                        &w,
                        Ev::ItemCompleted {
                            key: k,
                            status: ItemStatus::Completed,
                            body: None,
                        },
                    )
                    .await;
                }
                Step::Withdraw => {
                    let request_id = format!("r{}", key());
                    emit(
                        &w,
                        Ev::Request {
                            request_id: request_id.clone(),
                            request: InteractionRequest::Approval {
                                title: "Temporary request".into(),
                                detail: None,
                                subject: Subject::Other {
                                    description: "withdrawn by the agent".into(),
                                },
                                options: vec![ApprovalOption {
                                    id: "allow".into(),
                                    label: "Allow".into(),
                                    kind: ApprovalOptionKind::AllowOnce,
                                }],
                            },
                            item_key: None,
                            background_key: None,
                        },
                    )
                    .await;
                    emit(&w, Ev::Withdraw { request_id }).await;
                }
                Step::Question => {
                    let request_id = format!("q{}", key());
                    emit(
                        &w,
                        Ev::Request {
                            request_id: request_id.clone(),
                            request: InteractionRequest::Question {
                                title: "Pick one".into(),
                                questions: vec![Question {
                                    id: "q1".into(),
                                    header: None,
                                    prompt: "Which color?".into(),
                                    choices: vec![
                                        QuestionChoice {
                                            id: "red".into(),
                                            label: "Red".into(),
                                            description: None,
                                        },
                                        QuestionChoice {
                                            id: "blue".into(),
                                            label: "Blue".into(),
                                            description: None,
                                        },
                                    ],
                                    multi_select: false,
                                    allow_free_text: true,
                                    placeholder: None,
                                }],
                            },
                            item_key: None,
                            background_key: None,
                        },
                    )
                    .await;
                    let Some(resolution) = ctx.wait_response(&request_id).await else {
                        break 'steps TurnStatus::Interrupted;
                    };
                    let answer = match resolution {
                        InteractionResolution::Question { answers } => answers
                            .iter()
                            .map(|a| a.text.clone().unwrap_or_else(|| a.choice_ids.join(",")))
                            .collect::<Vec<_>>()
                            .join(";"),
                        InteractionResolution::Dismissed => "(dismissed)".into(),
                        InteractionResolution::Approval { option_id, .. } => option_id,
                    };
                    let k = key();
                    emit(
                        &w,
                        Ev::ItemStarted {
                            key: k.clone(),
                            body: ItemBody::AgentMessage {
                                text: String::new(),
                            },
                        },
                    )
                    .await;
                    emit(
                        &w,
                        Ev::Delta {
                            key: k.clone(),
                            field: DeltaField::Text,
                            text: format!("answer: {answer}"),
                        },
                    )
                    .await;
                    emit(
                        &w,
                        Ev::ItemCompleted {
                            key: k,
                            status: ItemStatus::Completed,
                            body: None,
                        },
                    )
                    .await;
                }
                Step::Plan => {
                    let k = key();
                    let entries = |done: usize| {
                        ["Reproduce", "Fix", "Verify"]
                            .iter()
                            .enumerate()
                            .map(|(i, t)| PlanEntry {
                                text: (*t).into(),
                                status: if i < done {
                                    PlanEntryStatus::Completed
                                } else if i == done {
                                    PlanEntryStatus::InProgress
                                } else {
                                    PlanEntryStatus::Pending
                                },
                            })
                            .collect::<Vec<_>>()
                    };
                    emit(
                        &w,
                        Ev::ItemStarted {
                            key: k.clone(),
                            body: ItemBody::Plan {
                                entries: entries(0),
                            },
                        },
                    )
                    .await;
                    for done in 1..=3 {
                        emit(
                            &w,
                            Ev::ItemUpdated {
                                key: k.clone(),
                                body: ItemBody::Plan {
                                    entries: entries(done),
                                },
                            },
                        )
                        .await;
                    }
                    emit(
                        &w,
                        Ev::ItemCompleted {
                            key: k,
                            status: ItemStatus::Completed,
                            body: None,
                        },
                    )
                    .await;
                }
                Step::Write { path, content } => {
                    let k = key();
                    let full = ctx.options.cwd.join(&path);
                    let existed = full.exists();
                    let result = match full.parent() {
                        Some(parent) => std::fs::create_dir_all(parent)
                            .and_then(|_| std::fs::write(&full, format!("{content}\n"))),
                        None => std::fs::write(&full, format!("{content}\n")),
                    };
                    let change = FileChange {
                        path: path.clone(),
                        kind: if existed {
                            FileChangeKind::Update
                        } else {
                            FileChangeKind::Add
                        },
                        move_path: None,
                        diff: Some(format!("+{content}\n")),
                        added: Some(1),
                        removed: Some(0),
                    };
                    emit(
                        &w,
                        Ev::ItemStarted {
                            key: k.clone(),
                            body: ItemBody::FileChange {
                                changes: vec![change],
                            },
                        },
                    )
                    .await;
                    let status = if result.is_ok() {
                        ItemStatus::Completed
                    } else {
                        ItemStatus::Failed
                    };
                    emit(
                        &w,
                        Ev::ItemCompleted {
                            key: k,
                            status,
                            body: None,
                        },
                    )
                    .await;
                }
                Step::Sleep(ms) => {
                    if !ctx.pause(ms).await {
                        break 'steps TurnStatus::Interrupted;
                    }
                }
                Step::Fail(message) => {
                    ctx.finish(
                        TurnStatus::Failed,
                        None,
                        Some(TurnError {
                            message,
                            kind: "harnessError".into(),
                        }),
                    )
                    .await;
                    return TurnEnd::Done;
                }
                Step::Crash(code) => {
                    // What the turn did so far stays in the transcript, as with a CLI that
                    // writes as it goes.
                    ctx.save(TurnStatus::Failed).await;
                    return TurnEnd::Crash(code);
                }
                Step::Hang(ms) => {
                    // Deliberately deaf to interrupts (and, in `run_agent`, to the end of the
                    // input): the forced stop is the only way out.
                    ctx.hanging.store(true, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(ms)).await;
                    ctx.hanging.store(false, Ordering::SeqCst);
                }
                Step::Context { used, window } => {
                    context = Some(ContextUsage {
                        used_tokens: used,
                        window_tokens: window,
                    });
                    emit(
                        &w,
                        Ev::Usage {
                            usage: usage_now(output_tokens, context),
                        },
                    )
                    .await;
                }
                Step::Background(spec) => {
                    let item = spec.launch_item.then(&mut key);
                    if let Some(k) = &item {
                        let cwd = ctx.options.cwd.display().to_string();
                        emit(
                            &w,
                            Ev::ItemStarted {
                                key: k.clone(),
                                body: spec.launch_body(&cwd),
                            },
                        )
                        .await;
                    }
                    // The task is reported before its launching item is closed.
                    ctx.background.start(&w, spec, item.clone()).await;
                    if let Some(k) = item {
                        emit(
                            &w,
                            Ev::ItemCompleted {
                                key: k,
                                status: ItemStatus::Backgrounded,
                                body: None,
                            },
                        )
                        .await;
                    }
                }
                Step::Unknown(name) => {
                    emit(
                        &w,
                        Ev::Notice {
                            level: NoticeLevel::Warning,
                            message: format!("unknown directive @{name}"),
                        },
                    )
                    .await;
                }
                Step::Invalid(message) => {
                    emit(
                        &w,
                        Ev::Notice {
                            level: NoticeLevel::Warning,
                            message,
                        },
                    )
                    .await;
                }
            }
        }
        TurnStatus::Completed
    };

    for k in open_items {
        emit(
            &w,
            Ev::ItemCompleted {
                key: k,
                status: ItemStatus::Interrupted,
                body: None,
            },
        )
        .await;
    }
    let usage = usage_now(output_tokens, context);
    emit(&w, Ev::Usage { usage }).await;
    ctx.finish(status, Some(usage), None).await;
    TurnEnd::Done
}

fn command_body(
    command: &str,
    options: &AgentOptions,
    output: &str,
    exit: Option<i32>,
) -> ItemBody {
    ItemBody::CommandExecution {
        command: command.to_owned(),
        cwd: Some(options.cwd.display().to_string()),
        output: output.to_owned(),
        output_truncated: false,
        output_blob_id: None,
        exit_code: exit,
        duration_ms: exit.map(|_| 1),
    }
}

/// Entry point for a standalone fake agent process (used by `aas-dummy-agent agent`).
pub async fn run_stdio(options: AgentOptions) -> i32 {
    run_agent(tokio::io::stdin(), tokio::io::stdout(), options).await
}

/// Buffer of the in-memory pipes between [`record_session`] and its agent. A pure buffer size:
/// both ends are read continuously.
const RECORDER_PIPE_BYTES: usize = 64 * 1024;

/// The answer a scripted user gives: approvals are allowed once (the first allow-once option,
/// else the first option), questions get the first choice of each question.
fn scripted_answer(request: &InteractionRequest) -> InteractionResolution {
    match request {
        InteractionRequest::Approval { options, .. } => InteractionResolution::Approval {
            option_id: options
                .iter()
                .find(|o| o.kind == ApprovalOptionKind::AllowOnce)
                .or(options.first())
                .map(|o| o.id.clone())
                .unwrap_or_default(),
            feedback: None,
        },
        InteractionRequest::Question { questions, .. } => InteractionResolution::Question {
            answers: questions
                .iter()
                .map(|q| QuestionAnswer {
                    question_id: q.id.clone(),
                    choice_ids: q
                        .choices
                        .first()
                        .map(|c| vec![c.id.clone()])
                        .unwrap_or_default(),
                    text: None,
                })
                .collect(),
        },
    }
}

/// Records a session the way someone using the CLI on the PC would: runs the agent in `cwd`
/// with its session store in `sessions_dir`, sends `prompts` as consecutive turns (a scripted
/// user answers requests, see [`scripted_answer`]), and returns the new session's id. It
/// prepares native sessions to list and import (`aas-test-server`). Scenarios that wait
/// (`@sleep`, `@hang`) make it wait as long, so callers bound it.
pub async fn record_session(
    sessions_dir: &std::path::Path,
    cwd: &std::path::Path,
    prompts: &[String],
) -> std::io::Result<String> {
    let (ops, agent_in) = tokio::io::duplex(RECORDER_PIPE_BYTES);
    let (agent_out, events) = tokio::io::duplex(RECORDER_PIPE_BYTES);
    let options = AgentOptions {
        cwd: cwd.to_path_buf(),
        chunk_size: DEFAULT_CHUNK_SIZE,
    };
    let agent = tokio::spawn(run_agent(agent_in, agent_out, options));
    let mut ops = JsonLinesWriter::new(ops);
    let mut events = JsonLinesReader::new(events, MAX_OP_LINE_BYTES);
    async fn next(events: &mut JsonLinesReader<tokio::io::DuplexStream>) -> std::io::Result<Ev> {
        match events.next().await.map_err(std::io::Error::other)? {
            Some(ReadLine::Json(v)) => serde_json::from_value(v).map_err(std::io::Error::other),
            Some(ReadLine::NotJson(line)) => Err(std::io::Error::other(format!(
                "the agent wrote a line that is not JSON: {line}"
            ))),
            None => Err(std::io::Error::other("the agent exited while recording")),
        }
    }
    let id = uuid::Uuid::new_v4().to_string();
    ops.send(&Op::Hello {
        session_id: id.clone(),
        resume: false,
        fork_from: None,
        sessions_dir: Some(sessions_dir.display().to_string()),
    })
    .await?;
    loop {
        match next(&mut events).await? {
            Ev::Ready { .. } => break,
            Ev::Rejected { message } => return Err(std::io::Error::other(message)),
            _ => {}
        }
    }
    for prompt in prompts {
        ops.send(&Op::Prompt {
            text: prompt.clone(),
            images: Vec::new(),
        })
        .await?;
        loop {
            match next(&mut events).await? {
                Ev::Request {
                    request_id,
                    request,
                    ..
                } => {
                    ops.send(&Op::Respond {
                        request_id,
                        resolution: scripted_answer(&request),
                    })
                    .await?
                }
                Ev::TurnCompleted { .. } => break,
                _ => {}
            }
        }
    }
    // The end of its input ends the agent.
    ops.close().await?;
    let code = agent.await.map_err(std::io::Error::other)?;
    if code != 0 {
        return Err(std::io::Error::other(format!(
            "the agent exited with code {code} while recording"
        )));
    }
    Ok(id)
}

/// A ready-to-send JSON line for scripted tests.
pub fn op_line(op: &Op) -> String {
    let mut s = serde_json::to_string(op).expect("op serializes");
    s.push('\n');
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scripts_parse_directives_and_mentions() {
        assert_eq!(
            parse_script("hello"),
            vec![Step::Text("echo: hello".into())]
        );
        assert_eq!(
            parse_script("look at @src/main.rs\n@stream 3 5\n@approve cargo test\n@crash"),
            vec![
                Step::Text("look at @src/main.rs".into()),
                Step::Stream {
                    count: 3,
                    interval_ms: 5
                },
                Step::Approve("cargo test".into()),
                Step::Crash(3),
            ]
        );
        assert_eq!(
            parse_script("@write a/b.txt hi there"),
            vec![Step::Write {
                path: "a/b.txt".into(),
                content: "hi there".into()
            }]
        );
        assert_eq!(
            parse_script("@src/lib.rs please"),
            vec![Step::Text("echo: @src/lib.rs please".into())]
        );
        assert_eq!(
            parse_script("@context 1200 8000"),
            vec![Step::Context {
                used: 1200,
                window: 8000
            }]
        );
        assert_eq!(
            parse_script("@hang 250\n@hang"),
            vec![Step::Hang(250), Step::Hang(DEFAULT_HANG_MS)]
        );
    }

    /// Runs the agent on in-memory pipes; returns the op sender, the event receiver and the
    /// agent task.
    fn start() -> (
        tokio::io::DuplexStream,
        tokio::io::Lines<tokio::io::BufReader<tokio::io::DuplexStream>>,
        tokio::task::JoinHandle<i32>,
    ) {
        use tokio::io::AsyncBufReadExt;
        let (ops, agent_in) = tokio::io::duplex(1 << 16);
        let (agent_out, events) = tokio::io::duplex(1 << 16);
        let task = tokio::spawn(run_agent(agent_in, agent_out, AgentOptions::default()));
        (ops, tokio::io::BufReader::new(events).lines(), task)
    }

    async fn send(ops: &mut tokio::io::DuplexStream, op: &Op) {
        use tokio::io::AsyncWriteExt;
        ops.write_all(op_line(op).as_bytes()).await.unwrap();
    }

    async fn next_ev(
        events: &mut tokio::io::Lines<tokio::io::BufReader<tokio::io::DuplexStream>>,
    ) -> Ev {
        let line = tokio::time::timeout(Duration::from_secs(10), events.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        serde_json::from_str(&line).unwrap()
    }

    #[tokio::test]
    async fn a_hanging_turn_ignores_interrupts_and_the_end_of_its_input() {
        let (mut ops, mut events, task) = start();
        send(
            &mut ops,
            &Op::Prompt {
                text: "@hang 600000".into(),
                images: Vec::new(),
            },
        )
        .await;
        assert_eq!(
            next_ev(&mut events).await,
            Ev::PromptAck {
                accepted: true,
                own_run: false
            }
        );
        assert_eq!(next_ev(&mut events).await, Ev::TurnStarted);
        send(&mut ops, &Op::Interrupt).await;
        drop(ops);
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            !task.is_finished(),
            "neither the interrupt nor the closed input ended the agent"
        );
        task.abort();
    }

    #[tokio::test]
    async fn after_a_short_hang_the_turn_notices_the_interrupt() {
        let (mut ops, mut events, task) = start();
        send(
            &mut ops,
            &Op::Prompt {
                text: "@hang 200\n@text never shown".into(),
                images: Vec::new(),
            },
        )
        .await;
        assert_eq!(
            next_ev(&mut events).await,
            Ev::PromptAck {
                accepted: true,
                own_run: false
            }
        );
        assert_eq!(next_ev(&mut events).await, Ev::TurnStarted);
        send(&mut ops, &Op::Interrupt).await;
        let status = loop {
            if let Ev::TurnCompleted { status, .. } = next_ev(&mut events).await {
                break status;
            }
        };
        assert_eq!(
            status,
            TurnStatus::Interrupted,
            "the interrupt counts once the hang is over"
        );
        drop(ops);
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(10), task)
                .await
                .unwrap()
                .unwrap(),
            0
        );
    }

    /// The turn is over for the agent before its completion is reported: a prompt sent the
    /// moment the completion arrives always starts the next turn.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_prompt_sent_on_completion_is_never_refused() {
        let (mut ops, mut events, task) = start();
        for round in 0..200 {
            send(
                &mut ops,
                &Op::Prompt {
                    text: format!("@text round {round}"),
                    images: Vec::new(),
                },
            )
            .await;
            loop {
                match next_ev(&mut events).await {
                    Ev::TurnCompleted { status, .. } => {
                        assert_eq!(status, TurnStatus::Completed);
                        break;
                    }
                    Ev::PromptAck {
                        accepted: false, ..
                    } => panic!("round {round}: the prompt was refused"),
                    Ev::Notice { message, .. } => panic!("round {round}: {message}"),
                    _ => {}
                }
            }
        }
        drop(ops);
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(10), task)
                .await
                .unwrap()
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn a_hello_naming_no_session_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let (mut ops, mut events, task) = start();
        send(
            &mut ops,
            &Op::Hello {
                session_id: "missing".into(),
                resume: true,
                fork_from: None,
                sessions_dir: Some(dir.path().display().to_string()),
            },
        )
        .await;
        assert!(
            matches!(next_ev(&mut events).await, Ev::Rejected { message } if message.contains("missing"))
        );
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(10), task)
                .await
                .unwrap()
                .unwrap(),
            REJECTED_EXIT_CODE
        );

        // Without a store there is nothing to branch off.
        let (mut ops, mut events, _task) = start();
        send(
            &mut ops,
            &Op::Hello {
                session_id: "new".into(),
                resume: false,
                fork_from: Some("source".into()),
                sessions_dir: None,
            },
        )
        .await;
        assert!(matches!(next_ev(&mut events).await, Ev::Rejected { .. }));
    }
}
