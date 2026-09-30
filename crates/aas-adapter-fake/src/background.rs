//! Background work of the fake agent (`@bg`): tasks that run beside and after turns, the way
//! Claude Code's background agents and shells or Codex's background terminals do.
//!
//! The agent reports each task with its whole state (`Ev::Background`), keeps the live set in
//! the task's `live` flag (live while running), ends a task only when its scenario says so or
//! when it is stopped (`Op::StopBackground`), and can start a turn by itself when a task ends
//! (`wake`, like Claude's task notifications). Tasks die with the agent.
//!
//! A task can print output while it runs (`output=`, reported as `Ev::BackgroundOutput`), and
//! a scheduled wakeup (`@wakeup`) starts a turn that runs its prompt when it comes due. The
//! same runtime holds the agent's questions outside turns (`@dialog`).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use aas_harness::{BackgroundOutcome, BackgroundState, BackgroundTaskInfo, BackgroundTasks};
use aas_protocol::types::*;
use tokio::io::AsyncWrite;
use tokio::sync::{mpsc, oneshot, watch};

use crate::agent::{Emitter, emit};
use crate::wire::Ev;

/// Default `ms=` of `@bg`: how long a task runs before it ends by itself.
pub const DEFAULT_BACKGROUND_MS: u64 = 1000;
/// Time between two output lines of a task that runs until it is stopped (`ms=0`), whose run
/// has no length to spread them over.
const OUTPUT_LINE_MS: u64 = 50;
/// Agents a workflow task reports in its progress.
const WORKFLOW_AGENTS: usize = 2;

/// How a background task ends when nobody stops it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackgroundEnd {
    Completed,
    Failed,
}

/// A background task a scenario starts: `@bg <key> [options…] [title…]` (see the directive
/// table of [`crate::agent`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackgroundSpec {
    pub key: String,
    pub kind: BackgroundTaskKind,
    pub title: String,
    /// How long each run lasts before it ends by itself; 0: until it is stopped.
    pub ms: u64,
    pub end: BackgroundEnd,
    /// Exit code reported with the result (shell tasks report 0 or 1 without it).
    pub exit: Option<i32>,
    /// Progress reports per run.
    pub progress: u32,
    pub parent: Option<String>,
    /// Further runs under the same key after the first one ended.
    pub restart: u32,
    pub ambient: bool,
    /// The agent starts a turn by itself when the task ends.
    pub wake: bool,
    /// The task asks for approval before it ends (a denial fails it).
    pub approve: bool,
    pub stoppable: bool,
    /// The task ignores stop requests (it still dies with the agent).
    pub stubborn: bool,
    /// Launched from an item of the turn (closed as `backgrounded`); `false` for `detached`.
    pub launch_item: bool,
    /// Lines each run prints (streamed as the task's output).
    pub output: u32,
    /// Bytes of each output line, line break included (padded); `None`: the line as it is.
    pub width: Option<usize>,
    /// Output lines the launching command prints while the turn runs (its item's output).
    pub early: u32,
    /// Output reported as snapshots of the whole output instead of appended text.
    pub snapshots: bool,
    /// A wakeup's prompt (`@wakeup`): the turn the agent starts when it comes due runs it.
    pub script: Option<String>,
}

impl Default for BackgroundSpec {
    fn default() -> Self {
        BackgroundSpec {
            key: String::new(),
            kind: BackgroundTaskKind::Agent,
            title: String::new(),
            ms: DEFAULT_BACKGROUND_MS,
            end: BackgroundEnd::Completed,
            exit: None,
            progress: 0,
            parent: None,
            restart: 0,
            ambient: false,
            wake: false,
            approve: false,
            stoppable: true,
            stubborn: false,
            launch_item: true,
            output: 0,
            width: None,
            early: 0,
            snapshots: false,
            script: None,
        }
    }
}

impl BackgroundSpec {
    /// Parses the arguments of `@bg`: the key, then options (`name=value` or flags), then the
    /// title (the rest of the line).
    pub fn parse(args: &str) -> Result<Self, String> {
        let mut words = args.split_whitespace();
        let key = words
            .next()
            .ok_or("@bg needs a key: @bg <key> [options…] [title…]")?
            .to_owned();
        let mut spec = BackgroundSpec {
            key,
            ..BackgroundSpec::default()
        };
        let mut title = Vec::new();
        for word in words {
            if !title.is_empty() {
                title.push(word);
                continue;
            }
            let number = |v: &str| -> Result<u64, String> {
                v.parse()
                    .map_err(|_| format!("@bg: `{word}` needs a number"))
            };
            match word.split_once('=') {
                Some(("kind", v)) => {
                    spec.kind = BackgroundTaskKind::parse(v)
                        .ok_or_else(|| format!("@bg: unknown kind `{v}`"))?
                }
                Some(("ms", v)) => spec.ms = number(v)?,
                Some(("end", "completed")) => spec.end = BackgroundEnd::Completed,
                Some(("end", "failed")) => spec.end = BackgroundEnd::Failed,
                Some(("end", v)) => return Err(format!("@bg: unknown end `{v}`")),
                Some(("exit", v)) => {
                    spec.exit = Some(
                        v.parse()
                            .map_err(|_| format!("@bg: `{word}` needs a number"))?,
                    )
                }
                Some(("progress", v)) => spec.progress = number(v)? as u32,
                Some(("parent", v)) => spec.parent = Some(v.to_owned()),
                Some(("restart", v)) => spec.restart = number(v)? as u32,
                Some(("output", v)) => spec.output = number(v)? as u32,
                Some(("width", v)) => spec.width = Some(number(v)? as usize),
                Some(("early", v)) => spec.early = number(v)? as u32,
                _ => match word {
                    "snapshots" => spec.snapshots = true,
                    "ambient" => spec.ambient = true,
                    "wake" => spec.wake = true,
                    "approve" => spec.approve = true,
                    "unstoppable" => spec.stoppable = false,
                    "stubborn" => spec.stubborn = true,
                    "detached" => spec.launch_item = false,
                    _ => title.push(word),
                },
            }
        }
        spec.title = if title.is_empty() {
            format!("{} {}", spec.kind.as_str(), spec.key)
        } else {
            title.join(" ")
        };
        if spec.early > spec.output {
            return Err(format!(
                "@bg: early={} is more than the output={} lines",
                spec.early, spec.output
            ));
        }
        if spec.early > 0 && (spec.kind != BackgroundTaskKind::Shell || !spec.launch_item) {
            return Err("@bg: early= needs kind=shell with a launching item".into());
        }
        Ok(spec)
    }

    /// Parses the arguments of `@wakeup <ms> [times=<n>] [prompt…]`: a scheduled wakeup like
    /// Claude Code's `ScheduleWakeup` (it cannot be stopped on its own), coming due `ms` after
    /// it is scheduled and then running `prompt` in a turn of the agent's own, `n` times in
    /// all. Its key is given when the turn schedules it (after the item that does).
    pub fn parse_wakeup(args: &str) -> Result<Self, String> {
        let usage = "@wakeup needs a delay: @wakeup <ms> [times=<n>] [prompt…]";
        let (ms, rest) = args.split_once(char::is_whitespace).unwrap_or((args, ""));
        let ms: u64 = ms.parse().map_err(|_| usage.to_owned())?;
        if ms == 0 {
            return Err(usage.into());
        }
        let rest = rest.trim();
        let (times, prompt) = match rest.split_once(char::is_whitespace) {
            Some((first, prompt)) if first.starts_with("times=") => (first, prompt.trim()),
            _ if rest.starts_with("times=") => (rest, ""),
            _ => ("times=1", rest),
        };
        let times: u32 = times["times=".len()..]
            .parse()
            .ok()
            .filter(|n| *n > 0)
            .ok_or("@wakeup: times= needs a number of at least 1")?;
        let prompt = if prompt.is_empty() { "woke up" } else { prompt };
        Ok(BackgroundSpec {
            kind: BackgroundTaskKind::Scheduled,
            title: prompt.to_owned(),
            ms,
            restart: times - 1,
            wake: true,
            stoppable: false,
            script: Some(prompt.to_owned()),
            ..BackgroundSpec::default()
        })
    }

    /// Output line `i` (from 1) of the task.
    fn output_line(&self, i: u32) -> String {
        let mut line = format!("{} line {i}", self.key);
        if let Some(width) = self.width {
            while line.len() + 1 < width {
                line.push('.');
            }
        }
        line.push('\n');
        line
    }

    /// What the launching command prints before it goes on in the background (`early=`).
    pub fn early_output(&self) -> String {
        (1..=self.early).map(|i| self.output_line(i)).collect()
    }

    /// The task's first report.
    pub fn info(&self, origin_item_key: Option<String>) -> BackgroundTaskInfo {
        BackgroundTaskInfo {
            origin_item_key,
            parent_key: self.parent.clone(),
            ambient: self.ambient,
            stoppable: self.stoppable,
            next_run_at: self.next_run_at(),
            ..BackgroundTaskInfo::new(self.key.clone(), self.kind, self.title.clone())
        }
    }

    /// A scheduled wakeup says when it comes due.
    fn next_run_at(&self) -> Option<Millis> {
        (self.kind == BackgroundTaskKind::Scheduled && self.ms > 0)
            .then(|| now_ms() + self.ms as i64)
    }

    /// The item of the turn that launches the task.
    pub fn launch_body(&self, cwd: &str) -> ItemBody {
        if self.script.is_some() {
            return ItemBody::ToolCall {
                category: ToolCategory::Other,
                name: "ScheduleWakeup".into(),
                title: self.title.clone(),
                server: None,
                input: None,
                output: Some(format!("wakeup scheduled in {} ms", self.ms)),
                output_truncated: false,
                output_blob_id: None,
            };
        }
        match self.kind {
            BackgroundTaskKind::Shell => ItemBody::CommandExecution {
                command: self.title.clone(),
                cwd: Some(cwd.to_owned()),
                output: String::new(),
                output_truncated: false,
                output_blob_id: None,
                exit_code: None,
                duration_ms: None,
            },
            kind => ItemBody::ToolCall {
                category: match kind {
                    BackgroundTaskKind::Agent | BackgroundTaskKind::Workflow => {
                        ToolCategory::Subagent
                    }
                    _ => ToolCategory::Other,
                },
                name: kind.as_str().to_owned(),
                title: self.title.clone(),
                server: None,
                input: None,
                output: Some(format!("started in the background as {}", self.key)),
                output_truncated: false,
                output_blob_id: None,
            },
        }
    }
}

fn now_ms() -> Millis {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as Millis)
        .unwrap_or(0)
}

/// A turn the agent starts by itself when a background task ends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Wake {
    pub text: String,
    pub trigger: TurnTrigger,
    /// A wakeup's prompt, which the turn runs instead of saying `text`.
    pub script: Option<String>,
}

/// The background tasks of one agent: their states, how to stop them, who waits for answers.
pub(crate) struct BackgroundRuntime {
    tasks: parking_lot::Mutex<BackgroundTasks>,
    stops: parking_lot::Mutex<HashMap<String, watch::Sender<bool>>>,
    requests: parking_lot::Mutex<HashMap<String, oneshot::Sender<InteractionResolution>>>,
    wakes: mpsc::UnboundedSender<Wake>,
    runners: parking_lot::Mutex<Vec<tokio::task::AbortHandle>>,
}

impl BackgroundRuntime {
    pub(crate) fn new(wakes: mpsc::UnboundedSender<Wake>) -> Arc<Self> {
        Arc::new(Self {
            tasks: parking_lot::Mutex::new(BackgroundTasks::new()),
            stops: parking_lot::Mutex::new(HashMap::new()),
            requests: parking_lot::Mutex::new(HashMap::new()),
            wakes,
            runners: parking_lot::Mutex::new(Vec::new()),
        })
    }

    /// Starts task `spec` (reported right away, as launched from item `origin_item_key`).
    pub(crate) async fn start<W>(
        self: &Arc<Self>,
        writer: &Emitter<W>,
        spec: BackgroundSpec,
        origin_item_key: Option<String>,
    ) where
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let started = self.tasks.lock().started(spec.info(origin_item_key));
        if let Some(task) = started {
            emit(writer, Ev::Background { task }).await;
        }
        let (stop_tx, stop_rx) = watch::channel(false);
        self.stops.lock().insert(spec.key.clone(), stop_tx);
        let runner = tokio::spawn(run(self.clone(), writer.detached(), spec, stop_rx));
        self.runners.lock().push(runner.abort_handle());
    }

    /// `Op::StopBackground`: whether a running task of that key was asked to stop.
    pub(crate) fn stop(&self, key: &str) -> bool {
        match self.stops.lock().get(key) {
            Some(tx) => {
                let _ = tx.send(true);
                true
            }
            None => false,
        }
    }

    /// Asks the question `title` outside any turn and task (`@dialog`, like a pi extension's
    /// dialog): the request names neither, and the answer is reported with a notice. It waits
    /// until it is answered or the agent ends.
    pub(crate) async fn ask_dialog<W>(self: &Arc<Self>, writer: &Emitter<W>, title: &str)
    where
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let request_id = format!("dialog-{}", uuid::Uuid::new_v4());
        let (tx, rx) = oneshot::channel();
        self.requests.lock().insert(request_id.clone(), tx);
        emit(
            writer,
            Ev::Request {
                request_id,
                request: InteractionRequest::Question {
                    title: title.to_owned(),
                    questions: vec![Question {
                        id: "choice".into(),
                        header: None,
                        prompt: title.to_owned(),
                        choices: vec![
                            QuestionChoice {
                                id: "yes".into(),
                                label: "Yes".into(),
                                description: None,
                            },
                            QuestionChoice {
                                id: "no".into(),
                                label: "No".into(),
                                description: None,
                            },
                        ],
                        multi_select: false,
                        allow_free_text: false,
                        placeholder: None,
                    }],
                },
                item_key: None,
                background_key: None,
            },
        )
        .await;
        let writer = writer.detached();
        let title = title.to_owned();
        let waiter = tokio::spawn(async move {
            let Ok(resolution) = rx.await else {
                return;
            };
            let answer = match resolution {
                InteractionResolution::Question { answers } => answers
                    .iter()
                    .flat_map(|a| a.choice_ids.iter().cloned())
                    .collect::<Vec<_>>()
                    .join(","),
                InteractionResolution::Dismissed => "dismissed".into(),
                InteractionResolution::Approval { option_id, .. } => option_id,
            };
            emit(
                &writer,
                Ev::Notice {
                    level: NoticeLevel::Info,
                    message: format!("dialog {title}: {answer}"),
                },
            )
            .await;
        });
        self.runners.lock().push(waiter.abort_handle());
    }

    /// Routes an answer to the background task or the dialog that asked; `false` when none
    /// did.
    pub(crate) fn answer(&self, request_id: &str, resolution: InteractionResolution) -> bool {
        match self.requests.lock().remove(request_id) {
            Some(tx) => {
                let _ = tx.send(resolution);
                true
            }
            None => false,
        }
    }

    /// Applies `change` to task `key` and reports the new state.
    async fn report<W>(
        &self,
        writer: &Emitter<W>,
        key: &str,
        change: impl FnOnce(&mut BackgroundTaskInfo),
    ) where
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let changed = self.tasks.lock().update(key, change);
        if let Some(task) = changed {
            emit(writer, Ev::Background { task }).await;
        }
    }
}

/// Held by the agent's main loop: background work dies with the agent (the runners hold the
/// runtime themselves, so dropping it would not end them).
pub(crate) struct BackgroundGuard(pub Arc<BackgroundRuntime>);

impl Drop for BackgroundGuard {
    fn drop(&mut self) {
        for runner in self.0.runners.lock().drain(..) {
            runner.abort();
        }
    }
}

/// Waits `ms` (forever for `None`) unless a stop comes first (ignored when `stubborn`).
/// Returns `false` when stopped.
async fn wait(ms: Option<u64>, stop: &mut watch::Receiver<bool>, stubborn: bool) -> bool {
    let sleep = async {
        match ms {
            Some(ms) => tokio::time::sleep(Duration::from_millis(ms)).await,
            None => std::future::pending().await,
        }
    };
    tokio::pin!(sleep);
    loop {
        tokio::select! {
            _ = &mut sleep => return true,
            changed = stop.changed(), if !stubborn => {
                if changed.is_err() || *stop.borrow() {
                    return false;
                }
            }
        }
    }
}

/// The progress of step `step` of `steps` of a run.
fn progress(
    spec: &BackgroundSpec,
    step: u32,
    steps: u32,
    started: std::time::Instant,
) -> BackgroundProgress {
    let workflow = if spec.kind == BackgroundTaskKind::Workflow {
        (0..WORKFLOW_AGENTS)
            .map(|i| WorkflowAgent {
                label: format!("agent-{}", i + 1),
                phase: Some("work".into()),
                state: if step == steps || (i == 0 && step * 2 > steps) {
                    WorkflowAgentState::Done
                } else if step == 1 {
                    WorkflowAgentState::Start
                } else {
                    WorkflowAgentState::Progress
                },
                agent_type: Some("general-purpose".into()),
                model: Some("fake-fast".into()),
                tokens: Some(100 * u64::from(step)),
            })
            .collect()
    } else {
        Vec::new()
    };
    BackgroundProgress {
        last_tool_name: Some("Bash".into()),
        tool_uses: Some(u64::from(step)),
        tokens: Some(100 * u64::from(step)),
        duration_ms: Some(started.elapsed().as_millis() as u64),
        summary: (spec.kind == BackgroundTaskKind::Workflow).then(|| spec.title.clone()),
        workflow,
    }
}

/// What a run produced (`printed`: what it printed before it ended).
fn result(spec: &BackgroundSpec, state: BackgroundState, printed: &str) -> BackgroundOutcome {
    let status = match state {
        BackgroundState::Running => "running",
        BackgroundState::Completed => "completed",
        BackgroundState::Failed => "failed",
        BackgroundState::Stopped => "stopped",
    };
    match spec.kind {
        BackgroundTaskKind::Shell => BackgroundOutcome {
            summary: None,
            exit_code: match state {
                BackgroundState::Stopped => None,
                _ => Some(spec.exit.unwrap_or(match state {
                    BackgroundState::Failed => 1,
                    _ => 0,
                })),
            },
            output: Some(format!("{printed}ran {}\n", spec.title)),
            output_omitted_bytes: None,
        },
        _ => BackgroundOutcome {
            summary: Some(format!("{} {status}", spec.title)),
            exit_code: None,
            output: None,
            output_omitted_bytes: None,
        },
    }
}

/// Runs one background task to its end (or until it is stopped), run after run.
async fn run<W>(
    rt: Arc<BackgroundRuntime>,
    writer: Emitter<W>,
    spec: BackgroundSpec,
    mut stop: watch::Receiver<bool>,
) where
    W: AsyncWrite + Unpin + Send + 'static,
{
    let key = spec.key.clone();
    for run in 0..=spec.restart {
        if run > 0 {
            // A new run under the same key (like an agent resumed by a notification).
            let restarted = rt.tasks.lock().started(spec.info(None));
            if let Some(task) = restarted {
                emit(&writer, Ev::Background { task }).await;
            }
            rt.report(&writer, &key, |t| t.live = true).await;
        }
        let started = std::time::Instant::now();
        // The first run's early lines came from the launching command.
        let first_line = if run == 0 { spec.early + 1 } else { 1 };
        let lines = spec.output + 1 - first_line;
        let slice =
            (spec.ms > 0).then(|| spec.ms / (u64::from(spec.progress) + u64::from(lines) + 1));
        let mut printed = if run == 0 {
            spec.early_output()
        } else {
            String::new()
        };
        let mut state = match spec.end {
            BackgroundEnd::Completed => BackgroundState::Completed,
            BackgroundEnd::Failed => BackgroundState::Failed,
        };
        'run: {
            for i in first_line..=spec.output {
                if !wait(slice.or(Some(OUTPUT_LINE_MS)), &mut stop, spec.stubborn).await {
                    state = BackgroundState::Stopped;
                    break 'run;
                }
                let line = spec.output_line(i);
                printed.push_str(&line);
                let (text, replace) = if spec.snapshots {
                    (printed.clone(), true)
                } else {
                    (line, false)
                };
                emit(
                    &writer,
                    Ev::BackgroundOutput {
                        key: key.clone(),
                        text,
                        replace,
                    },
                )
                .await;
            }
            for step in 1..=spec.progress {
                if !wait(slice, &mut stop, spec.stubborn).await {
                    state = BackgroundState::Stopped;
                    break 'run;
                }
                let progress = progress(&spec, step, spec.progress, started);
                rt.report(&writer, &key, |t| {
                    t.usage = Some(BackgroundUsage {
                        total_tokens: progress.tokens,
                        tool_uses: progress.tool_uses,
                        duration_ms: progress.duration_ms,
                        cost_usd: None,
                    });
                    t.progress = Some(progress);
                })
                .await;
            }
            if spec.approve && run == 0 {
                let request_id = format!("bg-{key}");
                let (tx, rx) = oneshot::channel();
                rt.requests.lock().insert(request_id.clone(), tx);
                emit(
                    &writer,
                    Ev::Request {
                        request_id: request_id.clone(),
                        request: InteractionRequest::Approval {
                            title: format!("Allow {}?", spec.title),
                            detail: Some("A background task asks.".into()),
                            subject: Subject::Tool {
                                name: spec.kind.as_str().to_owned(),
                                input: None,
                            },
                            options: vec![
                                ApprovalOption {
                                    id: "allow".into(),
                                    label: "Allow".into(),
                                    kind: ApprovalOptionKind::AllowOnce,
                                },
                                ApprovalOption {
                                    id: "deny".into(),
                                    label: "Deny".into(),
                                    kind: ApprovalOptionKind::Deny,
                                },
                            ],
                        },
                        item_key: None,
                        background_key: Some(key.clone()),
                    },
                )
                .await;
                // Stopped while it waits: the request goes with it (it ends unanswered).
                let answer = tokio::select! {
                    answer = rx => answer.ok(),
                    _ = wait(None, &mut stop, spec.stubborn) => None,
                };
                rt.requests.lock().remove(&request_id);
                match answer {
                    Some(InteractionResolution::Approval { option_id, .. })
                        if option_id == "allow" => {}
                    Some(_) => {
                        state = BackgroundState::Failed;
                        break 'run;
                    }
                    None => {
                        state = BackgroundState::Stopped;
                        break 'run;
                    }
                }
            }
            if !wait(slice, &mut stop, spec.stubborn).await {
                state = BackgroundState::Stopped;
            }
        }
        let outcome = result(&spec, state, &printed);
        rt.report(&writer, &key, |t| {
            t.live = false;
            t.state = state;
            t.result = Some(outcome);
            t.next_run_at = None;
        })
        .await;
        if spec.wake {
            let trigger = match spec.kind {
                BackgroundTaskKind::Scheduled => TurnTrigger::Scheduled,
                _ => TurnTrigger::BackgroundTask,
            };
            let status = match state {
                BackgroundState::Completed => "completed",
                BackgroundState::Failed => "failed",
                BackgroundState::Stopped => "was stopped",
                BackgroundState::Running => "runs",
            };
            // The main loop owns the receiver for the agent's whole life. A wakeup that was
            // stopped does not run its prompt.
            let _ = rt.wakes.send(Wake {
                text: format!("background task {key} {status}"),
                trigger,
                script: spec
                    .script
                    .clone()
                    .filter(|_| state != BackgroundState::Stopped),
            });
        }
        if state == BackgroundState::Stopped {
            break;
        }
        // A task that is stopped between runs does not start again.
        if *stop.borrow() && !spec.stubborn {
            break;
        }
    }
    rt.stops.lock().remove(&key);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directives_parse_options_flags_and_the_title() {
        let spec = BackgroundSpec::parse(
            "b1 kind=shell ms=0 exit=3 progress=2 parent=a1 restart=1 ambient wake approve unstoppable stubborn detached output=4 width=20 snapshots npm run dev",
        )
        .unwrap();
        assert_eq!(
            spec,
            BackgroundSpec {
                key: "b1".into(),
                kind: BackgroundTaskKind::Shell,
                title: "npm run dev".into(),
                ms: 0,
                end: BackgroundEnd::Completed,
                exit: Some(3),
                progress: 2,
                parent: Some("a1".into()),
                restart: 1,
                ambient: true,
                wake: true,
                approve: true,
                stoppable: false,
                stubborn: true,
                launch_item: false,
                output: 4,
                width: Some(20),
                early: 0,
                snapshots: true,
                script: None,
            }
        );
        assert_eq!(spec.output_line(3), "b1 line 3..........\n");
        assert_eq!(spec.output_line(3).len(), 20);
        let early = BackgroundSpec::parse("b kind=shell output=3 early=2 make").unwrap();
        assert_eq!(early.early_output(), "b line 1\nb line 2\n");
        assert!(BackgroundSpec::parse("b kind=shell output=1 early=2 make").is_err());
        assert!(BackgroundSpec::parse("b output=2 early=1 review").is_err());
        assert!(BackgroundSpec::parse("b kind=shell detached output=2 early=1 x").is_err());
        let plain = BackgroundSpec::parse("w kind=workflow end=failed").unwrap();
        assert_eq!(plain.title, "workflow w");
        assert_eq!(plain.ms, DEFAULT_BACKGROUND_MS);
        assert_eq!(plain.end, BackgroundEnd::Failed);
        // Once the title began, option-like words belong to it.
        assert_eq!(
            BackgroundSpec::parse("s echo ms=5").unwrap().title,
            "echo ms=5"
        );
        assert!(BackgroundSpec::parse("").is_err());
        assert!(BackgroundSpec::parse("x kind=nope").is_err());
        assert!(BackgroundSpec::parse("x ms=soon").is_err());
    }

    #[test]
    fn results_come_from_the_scenario_only() {
        let shell = BackgroundSpec::parse("b kind=shell make").unwrap();
        assert_eq!(
            result(&shell, BackgroundState::Completed, "").exit_code,
            Some(0)
        );
        assert_eq!(
            result(&shell, BackgroundState::Failed, "").exit_code,
            Some(1)
        );
        assert_eq!(result(&shell, BackgroundState::Stopped, "").exit_code, None);
        assert_eq!(
            result(&shell, BackgroundState::Completed, "b line 1\n")
                .output
                .as_deref(),
            Some("b line 1\nran make\n"),
            "the whole output: what it printed, then its last line"
        );
        let agent = BackgroundSpec::parse("a review").unwrap();
        assert_eq!(
            result(&agent, BackgroundState::Completed, "")
                .summary
                .as_deref(),
            Some("review completed")
        );
        let scheduled = BackgroundSpec::parse("s kind=scheduled ms=60000").unwrap();
        assert!(scheduled.info(None).next_run_at.is_some());
        // A wakeup: scheduled, unstoppable, due after its delay, running its prompt.
        let wakeup = BackgroundSpec::parse_wakeup("1500 times=3 @text woke").unwrap();
        assert_eq!(
            (
                wakeup.kind,
                wakeup.ms,
                wakeup.restart,
                wakeup.wake,
                wakeup.stoppable,
                wakeup.script.as_deref(),
                wakeup.title.as_str()
            ),
            (
                BackgroundTaskKind::Scheduled,
                1500,
                2,
                true,
                false,
                Some("@text woke"),
                "@text woke"
            )
        );
        assert!(wakeup.info(None).next_run_at.is_some());
        assert_eq!(
            BackgroundSpec::parse_wakeup("200")
                .unwrap()
                .script
                .as_deref(),
            Some("woke up")
        );
        assert_eq!(
            BackgroundSpec::parse_wakeup("200 check the build")
                .unwrap()
                .restart,
            0
        );
        assert!(BackgroundSpec::parse_wakeup("soon").is_err());
        assert!(BackgroundSpec::parse_wakeup("0 x").is_err());
        assert!(BackgroundSpec::parse_wakeup("10 times=0 x").is_err());
        let workflow = BackgroundSpec::parse("w kind=workflow progress=2").unwrap();
        let p = progress(&workflow, 2, 2, std::time::Instant::now());
        assert!(
            p.workflow
                .iter()
                .all(|a| a.state == WorkflowAgentState::Done)
        );
    }
}
