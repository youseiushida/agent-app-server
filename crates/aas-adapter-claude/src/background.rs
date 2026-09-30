//! The background work of one Claude Code process, mapped to the port's background tasks
//! ([`aas_harness::BackgroundTaskInfo`]) from explicit signals only (docs/adapters/claude.md §15):
//!
//! * **Tasks** (`local_agent`, `local_bash`, `local_workflow`, monitors, …): the live set is
//!   `system/background_tasks_changed` (a level signal that replaces the set with every
//!   message); starts, progress and ends are `system/task_started`, `task_progress`,
//!   `task_updated` and `task_notification`. A task is shown unless the CLI registered it in
//!   the foreground (`is_backgrounded: false`, the CLI's own rule for its task panel) and it
//!   never became background work (listed in the live set, or `task_updated.patch.
//!   is_backgrounded: true`).
//! * **Scheduled wakeups** (`CronCreate`, `ScheduleWakeup`, `/loop`): the CLI's complete list
//!   of pending wakeups is `session_crons` of the Stop hook input (sent at the end of every turn
//!   that ends normally) and the result of the `CronList` tool; `CronCreate` and `CronDelete`
//!   results name single wakeups, and a `ScheduleWakeup` result says when the one it scheduled
//!   comes due (`scheduledFor`, shown as `nextRunAt`). A wakeup is live from the result that
//!   created it, while that list holds it — and only while the list is known: a one-shot
//!   wakeup leaves the CLI's list when it fires, so after a turn that ended without the Stop
//!   hook while the CLI may have run it (it started a command of its own since the wakeup
//!   became known, [`Tracker::cli_command_started`]), the wakeup is no longer live until the
//!   next complete list names it ([`Tracker::turn_ended_without_list`]). Whether the CLI may
//!   have run a wakeup is never decided by comparing times with `scheduledFor`: the CLI's
//!   scheduler runs one-shot wakeups that land on :00 or :30 up to 90 s before it.
//! * **Output**: the shell a `local_bash` task runs writes to the file its `task_notification`
//!   names (`output_file`); the session reads it at that end ([`Tracker::output_file_of`]).
//!   Claude Code states no path while the task runs (`task_started` has none, and the Bash
//!   result names it only in its text for the model).
//!
//! The tracker never ends a task because of time and never reads text written for people.

use std::collections::{BTreeMap, HashMap};

use aas_harness::{
    BackgroundOutcome, BackgroundProgress, BackgroundState, BackgroundTaskInfo, BackgroundTaskKind,
    BackgroundTasks, BackgroundUsage, LiveEntry, WorkflowAgent, WorkflowAgentState,
};
use serde_json::Value;

/// Prefix of the keys of scheduled wakeups (`cron:<id>`): the CLI's cron ids and task ids are
/// separate namespaces.
pub(crate) const CRON_KEY_PREFIX: &str = "cron:";

/// Prefix of the key of the wakeup a `ScheduleWakeup` call scheduled (`wakeup:<tool use id>`):
/// its result gives no id, and its entry in the CLI's list is only known later.
pub(crate) const WAKEUP_KEY_PREFIX: &str = "wakeup:";

/// The task key of the scheduled wakeup `id`.
pub(crate) fn cron_key(id: &str) -> String {
    format!("{CRON_KEY_PREFIX}{id}")
}

/// Whether `listed`, the prompt of an entry of the CLI's list of wakeups, is `prompt`: the same
/// text, or the text the list gives for a long one ("Capped at 1000 chars; clipped values
/// append an in-string marker '… [+N chars]'", Claude Code's schema of `session_crons`: a
/// prefix of `prompt`, then the marker with the number of UTF-16 units left out).
pub(crate) fn listed_prompt_matches(listed: &str, prompt: &str) -> bool {
    if listed == prompt {
        return true;
    }
    let Some(marked) = listed.strip_suffix(" chars]") else {
        return false;
    };
    let Some((prefix, left_out)) = marked.rsplit_once("\u{2026} [+") else {
        return false;
    };
    let Ok(left_out) = left_out.parse::<usize>() else {
        return false;
    };
    prompt.starts_with(prefix)
        && prompt.encode_utf16().count() == prefix.encode_utf16().count() + left_out
}

/// Kind of a task by its `task_type` (the CLI's own table of task types, Claude Code 2.1.283:
/// `local_agent` subagent, `in_process_teammate` teammate, `local_workflow` workflow,
/// `local_bash` shell, `monitor_mcp` / `monitor_ws` monitor, `remote_agent` cloud session).
pub(crate) fn task_kind(task_type: &str) -> BackgroundTaskKind {
    match task_type {
        "local_agent" | "in_process_teammate" => BackgroundTaskKind::Agent,
        "local_workflow" => BackgroundTaskKind::Workflow,
        "local_bash" => BackgroundTaskKind::Shell,
        "monitor_mcp" | "monitor_ws" => BackgroundTaskKind::Monitor,
        "remote_agent" => BackgroundTaskKind::Remote,
        _ => BackgroundTaskKind::Other,
    }
}

fn str_field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

fn u64_field(v: &Value, key: &str) -> Option<u64> {
    v.get(key).and_then(Value::as_u64)
}

/// A task the CLI's tool result says it launched (see [`crate::mapping::launched_work`]).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Launch {
    pub task_id: String,
    pub kind: BackgroundTaskKind,
    pub title: String,
}

/// What a `ScheduleWakeup` call did (its structured result `{scheduledFor,
/// clampedDelaySeconds, wasClamped, stopped?, cancelledWakeups?}` and its input's `prompt` and
/// `reason`). Claude Code keeps one pending wakeup of the dynamic loop: every call removes the
/// pending ones before it schedules the next (2.1.283's implementation; `stop: true` removes
/// them and schedules none, and a loop that reached its maximum age schedules none either,
/// `scheduledFor: 0`).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Wakeup {
    /// When the scheduled wakeup comes due (epoch ms); `None`: none was scheduled.
    pub scheduled_for: Option<i64>,
    /// The prompt the CLI runs when it comes due.
    pub prompt: String,
    /// Why the model chose the delay, in its words ("shown to the user").
    pub reason: Option<String>,
}

/// A pending scheduled wakeup as the CLI lists it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Cron {
    pub id: String,
    /// The prompt the CLI runs when the wakeup comes due.
    pub prompt: String,
    /// The schedule as the CLI shows it: `humanSchedule` where the CLI gives one, else the
    /// cron expression.
    pub schedule: Option<String>,
    /// `schedule` is the CLI's `humanSchedule`.
    pub human_schedule: bool,
    /// The CLI says the wakeup fires on every match (`recurring: true`) and so stays in its
    /// list when it fires. Any other wakeup may be a one-shot one, which leaves the list when
    /// it fires ("False for one-shot wakeups whose cron field encodes a single fire time").
    pub recurring: bool,
}

impl Cron {
    /// An entry of `session_crons` (Stop hook: `id`, `schedule`, `recurring`, `prompt`) or of
    /// `CronList`'s `jobs` (`id`, `cron`, `humanSchedule`, `prompt`, `recurring`, …).
    pub(crate) fn from_entry(entry: &Value) -> Option<Self> {
        let human = str_field(entry, "humanSchedule");
        Some(Self {
            id: str_field(entry, "id")?.to_owned(),
            prompt: str_field(entry, "prompt").unwrap_or_default().to_owned(),
            schedule: human
                .or_else(|| str_field(entry, "schedule"))
                .or_else(|| str_field(entry, "cron"))
                .map(str::to_owned),
            human_schedule: human.is_some(),
            recurring: entry.get("recurring").and_then(Value::as_bool) == Some(true),
        })
    }
}

/// Who asks in a `can_use_tool` request that names an `agent_id`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Requester {
    /// A background task (its key): the request belongs to the task.
    Task(String),
    /// A subagent the turn runs in the foreground: the request belongs to the turn.
    Foreground,
    /// An agent the CLI never reported as a task.
    Unknown,
}

/// The CLI's details of a task that the port's task state does not carry.
#[derive(Debug, Default)]
struct Meta {
    tool_use_id: Option<String>,
    /// The agents of a workflow by their `index` (`workflow_progress` entries are merged by
    /// index, as the CLI merges them).
    workflow: BTreeMap<u64, WorkflowEntry>,
}

#[derive(Debug, Clone)]
struct WorkflowEntry {
    agent_id: Option<String>,
    agent: WorkflowAgent,
}

/// What was read of the file in which the CLI kept a task's output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OutputFile {
    /// The file's text (its end, when it was longer than the bound), invalid UTF-8 replaced.
    pub text: String,
    /// Bytes at the start that were not read.
    pub omitted: Option<u64>,
}

/// Reads the output file of a task: at most `max_bytes`, from the end when the file is longer
/// (the latest output, with the exit line the CLI appends), starting at a character boundary.
pub(crate) async fn read_output_file(
    path: &std::path::Path,
    max_bytes: u64,
) -> std::io::Result<OutputFile> {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};
    let mut file = tokio::fs::File::open(path).await?;
    let len = file.metadata().await?.len();
    let skip = len.saturating_sub(max_bytes);
    if skip > 0 {
        file.seek(std::io::SeekFrom::Start(skip)).await?;
    }
    let mut bytes = Vec::with_capacity(usize::try_from(len - skip).unwrap_or(0));
    // The file is not read past the bound even when it grew meanwhile.
    file.take(max_bytes).read_to_end(&mut bytes).await?;
    // A cut start may fall inside a UTF-8 sequence: its continuation bytes are left out too.
    let start = if skip > 0 {
        bytes
            .iter()
            .take(3)
            .take_while(|b| (**b & 0xC0) == 0x80)
            .count()
    } else {
        0
    };
    Ok(OutputFile {
        text: String::from_utf8_lossy(&bytes[start..]).into_owned(),
        omitted: (skip > 0).then_some(skip + start as u64),
    })
}

/// A pending scheduled wakeup as the tracker knows it.
#[derive(Debug, Clone)]
struct Pending {
    /// The wakeup. Its `id` is empty for the wakeup of a `ScheduleWakeup` until a list names it.
    cron: Cron,
    /// When the CLI said it comes due (`ScheduleWakeup`'s `scheduledFor`, epoch ms). Shown as
    /// `nextRunAt` only: the CLI may run the wakeup before it (see
    /// [`Tracker::cli_command_started`]).
    due_at: Option<i64>,
    /// The CLI started a command of its own since the wakeup was last known to be pending: a
    /// one-shot wakeup that fires leaves the CLI's list, so it may be gone.
    may_have_fired: bool,
    /// Not live until the next complete list says whether it is still pending (a one-shot
    /// wakeup that may have fired, after a turn that ended without the list).
    unconfirmed: bool,
}

impl Pending {
    fn listed(cron: Cron) -> Self {
        Self {
            cron,
            due_at: None,
            may_have_fired: false,
            unconfirmed: false,
        }
    }
}

/// An entry of the last `background_tasks_changed`.
#[derive(Debug, Clone)]
struct LiveTask {
    task_type: String,
    description: String,
    ambient: bool,
}

/// See the module docs.
#[derive(Debug, Default)]
pub(crate) struct Tracker {
    /// The tasks that are shown.
    tasks: BackgroundTasks,
    /// Tasks the CLI registered in the foreground: kept until they end, shown only if they
    /// become background work.
    hidden: HashMap<String, BackgroundTaskInfo>,
    meta: HashMap<String, Meta>,
    /// The last `background_tasks_changed`, by task id.
    live_tasks: BTreeMap<String, LiveTask>,
    /// Pending scheduled wakeups, by key.
    live_crons: BTreeMap<String, Pending>,
    /// Key of the pending wakeup of the dynamic loop (the one the last `ScheduleWakeup`
    /// scheduled; the CLI keeps one).
    loop_wakeup: Option<String>,
    /// Task id by the id of the tool use that started it.
    by_tool_use: HashMap<String, String>,
    /// Tool uses made inside a subagent that has not answered them yet: tool use id → the
    /// subagent's `parent_tool_use_id` (the tool use that started the subagent).
    sub_tools: HashMap<String, String>,
    /// Tasks owned by a subagent whose owner is not known yet, by their tool use id.
    unresolved: HashMap<String, String>,
}

/// Keeps the last state of each task, in the order the tasks first changed (a parent comes
/// before the tasks it launched).
fn merge_changes(changes: Vec<BackgroundTaskInfo>) -> Vec<BackgroundTaskInfo> {
    let mut out: Vec<BackgroundTaskInfo> = Vec::with_capacity(changes.len());
    for task in changes {
        match out.iter_mut().find(|t| t.key == task.key) {
            Some(slot) => *slot = task,
            None => out.push(task),
        }
    }
    out
}

fn cron_info(key: &str, pending: &Pending) -> BackgroundTaskInfo {
    let cron = &pending.cron;
    BackgroundTaskInfo {
        // Claude Code has no request that cancels a single wakeup.
        stoppable: false,
        progress: cron.schedule.as_ref().map(|s| BackgroundProgress {
            summary: Some(s.clone()),
            ..BackgroundProgress::default()
        }),
        next_run_at: pending.due_at,
        ..BackgroundTaskInfo::new(key, BackgroundTaskKind::Scheduled, cron.prompt.clone())
    }
}

fn workflow_state(state: &str) -> Option<WorkflowAgentState> {
    Some(match state {
        "start" => WorkflowAgentState::Start,
        "progress" => WorkflowAgentState::Progress,
        "done" => WorkflowAgentState::Done,
        "error" => WorkflowAgentState::Error,
        _ => return None,
    })
}

fn usage_of(usage: Option<&Value>) -> Option<BackgroundUsage> {
    let u = usage.filter(|u| u.is_object())?;
    Some(BackgroundUsage {
        total_tokens: u64_field(u, "total_tokens"),
        tool_uses: u64_field(u, "tool_uses"),
        duration_ms: u64_field(u, "duration_ms"),
        cost_usd: None,
    })
}

impl Tracker {
    /// A shown task.
    #[cfg(test)]
    pub(crate) fn get(&self, key: &str) -> Option<&BackgroundTaskInfo> {
        self.tasks.get(key)
    }

    /// Every shown task, ended ones included.
    #[cfg(test)]
    pub(crate) fn iter(&self) -> impl Iterator<Item = &BackgroundTaskInfo> {
        self.tasks.iter()
    }

    /// The owner (a task key) of the tool use `tool_use_id` made inside a subagent.
    fn owner_of(&self, tool_use_id: &str) -> Option<String> {
        let parent = self.sub_tools.get(tool_use_id)?;
        self.by_tool_use.get(parent).cloned()
    }

    /// Mirrors the live set: the tasks of the last `background_tasks_changed` and the pending
    /// wakeups the CLI's list still confirms.
    fn sync_live(&mut self) -> Vec<BackgroundTaskInfo> {
        let Self {
            tasks,
            live_tasks,
            live_crons,
            ..
        } = self;
        let entries: Vec<LiveEntry> = live_tasks
            .iter()
            .map(|(key, t)| LiveEntry {
                key: key.clone(),
                ambient: t.ambient,
            })
            .chain(
                live_crons
                    .iter()
                    .filter(|(_, p)| !p.unconfirmed)
                    .map(|(key, _)| LiveEntry {
                        key: key.clone(),
                        ambient: false,
                    }),
            )
            .collect();
        tasks.replace_live(entries, |entry| match live_tasks.get(&entry.key) {
            Some(t) => BackgroundTaskInfo {
                stoppable: true,
                ..BackgroundTaskInfo::new(
                    entry.key.clone(),
                    task_kind(&t.task_type),
                    t.description.clone(),
                )
            },
            None => match live_crons.get(&entry.key) {
                Some(pending) => cron_info(&entry.key, pending),
                // Every entry comes from one of the two sets.
                None => BackgroundTaskInfo::new(
                    entry.key.clone(),
                    BackgroundTaskKind::Other,
                    String::new(),
                ),
            },
        })
    }

    /// Shows a task that was kept hidden (it became background work).
    fn surface(&mut self, key: &str) -> Vec<BackgroundTaskInfo> {
        let Some(task) = self.hidden.remove(key) else {
            return Vec::new();
        };
        let live = self.live_tasks.get(key);
        let task = BackgroundTaskInfo {
            live: live.is_some(),
            ambient: live.map(|l| l.ambient).unwrap_or(task.ambient),
            ..task
        };
        self.tasks.started(task).into_iter().collect()
    }

    /// `system/background_tasks_changed {tasks: [{task_id, task_type, description, ambient?}]}`.
    pub(crate) fn live_changed(&mut self, msg: &Value) -> Vec<BackgroundTaskInfo> {
        let mut live = BTreeMap::new();
        for entry in msg
            .get("tasks")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let Some(id) = str_field(entry, "task_id") else {
                continue;
            };
            live.insert(
                id.to_owned(),
                LiveTask {
                    task_type: str_field(entry, "task_type").unwrap_or_default().to_owned(),
                    description: str_field(entry, "description")
                        .unwrap_or_default()
                        .to_owned(),
                    ambient: entry.get("ambient").and_then(Value::as_bool) == Some(true),
                },
            );
        }
        self.live_tasks = live;
        let mut changes = Vec::new();
        let listed: Vec<String> = self.live_tasks.keys().cloned().collect();
        for key in listed {
            changes.extend(self.surface(&key));
        }
        changes.extend(self.sync_live());
        merge_changes(changes)
    }

    /// `system/task_started`. `origin_item` is the key of the running turn's item for the
    /// task's `tool_use_id`, when there is one.
    pub(crate) fn started(
        &mut self,
        msg: &Value,
        origin_item: Option<String>,
    ) -> Vec<BackgroundTaskInfo> {
        let Some(id) = str_field(msg, "task_id").map(str::to_owned) else {
            return Vec::new();
        };
        let tool_use_id = str_field(msg, "tool_use_id").map(str::to_owned);
        let owned = msg.get("owned_by_subagent").and_then(Value::as_bool) == Some(true);
        let backgrounded = msg.get("is_backgrounded").and_then(Value::as_bool);
        if let Some(t) = &tool_use_id {
            self.by_tool_use.insert(t.clone(), id.clone());
        }
        let meta = self.meta.entry(id.clone()).or_default();
        if tool_use_id.is_some() {
            meta.tool_use_id = tool_use_id.clone();
        }
        // A tool use made inside a subagent names its owner; one the CLI marks as owned by a
        // subagent whose tool use is not known yet gets it when the tool use comes.
        let parent_key = tool_use_id.as_ref().and_then(|t| self.owner_of(t));
        if let (Some(t), true, None) = (&tool_use_id, owned, &parent_key) {
            self.unresolved.insert(t.clone(), id.clone());
        }
        let live = self.live_tasks.get(&id);
        let task = BackgroundTaskInfo {
            live: live.is_some(),
            ambient: live
                .map(|l| l.ambient)
                .unwrap_or(msg.get("ambient").and_then(Value::as_bool) == Some(true)),
            stoppable: true,
            origin_item_key: origin_item,
            parent_key,
            ..BackgroundTaskInfo::new(
                id.clone(),
                task_kind(str_field(msg, "task_type").unwrap_or_default()),
                str_field(msg, "description").unwrap_or_default(),
            )
        };
        let shown = self.tasks.get(&id).is_some() || live.is_some() || backgrounded != Some(false);
        if !shown {
            self.hidden.insert(id, task);
            return Vec::new();
        }
        let mut changes = self.surface(&id);
        changes.extend(self.tasks.started(task));
        merge_changes(changes)
    }

    /// `system/task_progress`.
    pub(crate) fn progress(&mut self, msg: &Value) -> Vec<BackgroundTaskInfo> {
        let Some(id) = str_field(msg, "task_id") else {
            return Vec::new();
        };
        let meta = self.meta.entry(id.to_owned()).or_default();
        for entry in msg
            .get("workflow_progress")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|e| str_field(e, "type") == Some("workflow_agent"))
        {
            let (Some(index), Some(label), Some(state)) = (
                u64_field(entry, "index"),
                str_field(entry, "label"),
                str_field(entry, "state").and_then(workflow_state),
            ) else {
                tracing::debug!(entry = %entry, "a workflow agent entry without index, label or a known state; skipped");
                continue;
            };
            meta.workflow.insert(
                index,
                WorkflowEntry {
                    agent_id: str_field(entry, "agentId").map(str::to_owned),
                    agent: WorkflowAgent {
                        label: label.to_owned(),
                        phase: str_field(entry, "phaseTitle").map(str::to_owned),
                        state,
                        agent_type: str_field(entry, "agentType").map(str::to_owned),
                        model: str_field(entry, "model").map(str::to_owned),
                        tokens: u64_field(entry, "tokens"),
                    },
                },
            );
        }
        let usage = msg.get("usage");
        let progress = BackgroundProgress {
            last_tool_name: str_field(msg, "last_tool_name").map(str::to_owned),
            tool_uses: usage.and_then(|u| u64_field(u, "tool_uses")),
            tokens: usage.and_then(|u| u64_field(u, "total_tokens")),
            duration_ms: usage.and_then(|u| u64_field(u, "duration_ms")),
            summary: str_field(msg, "summary")
                .or_else(|| str_field(msg, "description"))
                .map(str::to_owned),
            workflow: meta.workflow.values().map(|w| w.agent.clone()).collect(),
        };
        if let Some(hidden) = self.hidden.get_mut(id) {
            hidden.progress = Some(progress);
            return Vec::new();
        }
        self.tasks
            .update(id, |t| t.progress = Some(progress))
            .into_iter()
            .collect()
    }

    /// `system/task_updated {patch: {status?, description?, is_backgrounded?, …}}`.
    pub(crate) fn updated(&mut self, msg: &Value) -> Vec<BackgroundTaskInfo> {
        let Some(id) = str_field(msg, "task_id").map(str::to_owned) else {
            return Vec::new();
        };
        let Some(patch) = msg.get("patch") else {
            return Vec::new();
        };
        let mut changes = Vec::new();
        if patch.get("is_backgrounded").and_then(Value::as_bool) == Some(true) {
            changes.extend(self.surface(&id));
        }
        let end = match str_field(patch, "status") {
            Some("completed") => Some(BackgroundState::Completed),
            Some("failed") => Some(BackgroundState::Failed),
            Some("killed") => Some(BackgroundState::Stopped),
            // pending, running, paused: the task has not ended.
            _ => None,
        };
        let title = str_field(patch, "description").map(str::to_owned);
        let apply = |t: &mut BackgroundTaskInfo| {
            if let Some(state) = end {
                t.state = state;
            }
            if let Some(title) = &title {
                t.title = title.clone();
            }
        };
        if let Some(hidden) = self.hidden.get_mut(&id) {
            apply(hidden);
        } else {
            changes.extend(self.tasks.update(&id, apply));
        }
        merge_changes(changes)
    }

    /// The file whose content is the output of the task a `task_notification` ends: its
    /// `output_file`, for a shown shell task (`local_bash`: the file holds what the shell
    /// printed, and the lines the CLI adds, like the exit line). The file of an agent is its
    /// transcript, which is not output; a foreground task is not shown.
    pub(crate) fn output_file_of(&self, msg: &Value) -> Option<std::path::PathBuf> {
        let id = str_field(msg, "task_id")?;
        let task = self.tasks.get(id)?;
        if task.kind != BackgroundTaskKind::Shell || self.hidden.contains_key(id) {
            return None;
        }
        str_field(msg, "output_file")
            .filter(|f| !f.is_empty())
            .map(std::path::PathBuf::from)
    }

    /// `system/task_notification {status: completed|failed|stopped, summary, usage?,
    /// output_file}`: the end of a run, with the output read from `output_file` when the
    /// session read it ([`Self::output_file_of`]). Returns the changes and whether the task was
    /// known.
    pub(crate) fn notification(
        &mut self,
        msg: &Value,
        output: Option<OutputFile>,
    ) -> (Vec<BackgroundTaskInfo>, bool) {
        let Some(id) = str_field(msg, "task_id").map(str::to_owned) else {
            return (Vec::new(), false);
        };
        let state = match str_field(msg, "status") {
            Some("completed") => BackgroundState::Completed,
            Some("failed") => BackgroundState::Failed,
            Some("stopped") => BackgroundState::Stopped,
            other => {
                tracing::debug!(task = %id, status = ?other, "a task notification with an unknown status; ignored");
                return (Vec::new(), true);
            }
        };
        if self.hidden.remove(&id).is_some() {
            // A foreground task ended; it is never shown and never starts again under this id
            // (a resumed subagent registers in the background, as a shown task).
            self.forget(&id);
            return (Vec::new(), true);
        }
        let summary = str_field(msg, "summary").map(str::to_owned);
        let usage = usage_of(msg.get("usage"));
        let known = self.tasks.get(&id).is_some();
        let changes = self
            .tasks
            .update(&id, |t| {
                t.state = state;
                let (output, output_omitted_bytes) = match output {
                    Some(file) => (Some(file.text), file.omitted),
                    None => (None, None),
                };
                t.result = Some(BackgroundOutcome {
                    summary,
                    exit_code: None,
                    output,
                    output_omitted_bytes,
                });
                if usage.is_some() {
                    t.usage = usage;
                }
            })
            .into_iter()
            .collect();
        (changes, known)
    }

    fn forget(&mut self, id: &str) {
        if let Some(meta) = self.meta.remove(id)
            && let Some(t) = meta.tool_use_id
        {
            self.by_tool_use.remove(&t);
            self.unresolved.remove(&t);
        }
    }

    /// A tool result of the running turn says the tool launched task `launch` (its item
    /// `origin_item` goes on as the task).
    pub(crate) fn launched(
        &mut self,
        launch: Launch,
        origin_item: String,
    ) -> Vec<BackgroundTaskInfo> {
        let mut changes = self.surface(&launch.task_id);
        if self.tasks.get(&launch.task_id).is_some() {
            changes.extend(self.tasks.update(&launch.task_id, |t| {
                t.origin_item_key.get_or_insert(origin_item);
            }));
        } else {
            let live = self.live_tasks.get(&launch.task_id);
            changes.extend(self.tasks.started(BackgroundTaskInfo {
                live: live.is_some(),
                ambient: live.is_some_and(|l| l.ambient),
                stoppable: true,
                origin_item_key: Some(origin_item),
                ..BackgroundTaskInfo::new(launch.task_id, launch.kind, launch.title)
            }));
        }
        merge_changes(changes)
    }

    /// Whether a task was reported for the CLI's wakeup `id` (created by `CronCreate`, named by
    /// a list, or the `ScheduleWakeup` wakeup a list named).
    fn known_cron_id(&self, id: &str) -> bool {
        self.tasks.get(&cron_key(id)).is_some()
            || self
                .loop_wakeup
                .as_ref()
                .and_then(|k| self.live_crons.get(k))
                .is_some_and(|p| p.cron.id == id)
    }

    /// The key a wakeup of the CLI's list goes under: the `ScheduleWakeup` wakeup's once the
    /// list named it, else `cron:<id>`.
    fn key_of_cron(&self, id: &str) -> String {
        match &self.loop_wakeup {
            Some(key) if self.live_crons.get(key).is_some_and(|p| p.cron.id == id) => key.clone(),
            _ => cron_key(id),
        }
    }

    /// Ends pending wakeup `key` (it left the CLI's list): as fired (`completed`) when the CLI
    /// may have run it, as cancelled (`stopped`) when it cannot have run it yet.
    fn end_wakeup(&mut self, key: &str, fired: bool) -> Vec<BackgroundTaskInfo> {
        if self.loop_wakeup.as_deref() == Some(key) {
            self.loop_wakeup = None;
        }
        self.live_crons.remove(key);
        let state = if fired {
            BackgroundState::Completed
        } else {
            BackgroundState::Stopped
        };
        self.tasks
            .update(key, |t| {
                if !t.state.is_ended() {
                    t.state = state;
                }
            })
            .into_iter()
            .collect()
    }

    /// The complete list of pending wakeups (Stop hook `session_crons`, `CronList` result).
    /// A listed wakeup is live; one that is no longer listed is not pending any more.
    ///
    /// The wakeup `ScheduleWakeup` scheduled is in the list as a one-shot entry with the
    /// call's prompt (the CLI stores it as it came, and keeps one pending wakeup of the dynamic
    /// loop); the first list with exactly one such entry that no task was reported for names
    /// it — while the CLI cannot have run it yet: once it may have (a command of the CLI's own
    /// started since it became known), an entry with its prompt can be the next wakeup of the
    /// loop (the one the CLI arms by itself when the model did not reschedule), which is a
    /// wakeup of its own. A list that holds none of them no longer holds it: fired when the
    /// CLI may have run it, cancelled otherwise — unless no list ever named it and the CLI
    /// cannot have run it yet (its entry was not recognized), when it stays as it is.
    pub(crate) fn crons_listed(&mut self, crons: Vec<Cron>) -> Vec<BackgroundTaskInfo> {
        if let Some(key) = self.loop_wakeup.clone()
            && let Some(pending) = self.live_crons.get(&key)
            && pending.cron.id.is_empty()
            && !pending.may_have_fired
        {
            let candidates: Vec<&Cron> = crons
                .iter()
                .filter(|c| {
                    !c.recurring
                        && !self.known_cron_id(&c.id)
                        && listed_prompt_matches(&c.prompt, &pending.cron.prompt)
                })
                .collect();
            match candidates.as_slice() {
                [one] => {
                    let id = one.id.clone();
                    if let Some(pending) = self.live_crons.get_mut(&key) {
                        pending.cron.id = id;
                    }
                }
                [] => {}
                several => {
                    tracing::warn!(wakeup = %key, entries = several.len(), "several entries of the CLI's list of wakeups may be the one ScheduleWakeup scheduled; it waits for a list that tells them apart");
                }
            }
        }
        let listed: BTreeMap<String, Cron> = crons
            .into_iter()
            .map(|c| (self.key_of_cron(&c.id), c))
            .collect();
        let mut changes = Vec::new();
        let gone: Vec<(String, Pending)> = self
            .live_crons
            .iter()
            .filter(|(k, _)| !listed.contains_key(*k))
            .map(|(k, p)| (k.clone(), p.clone()))
            .collect();
        let mut kept = BTreeMap::new();
        for (key, pending) in gone {
            if pending.cron.id.is_empty() && !pending.may_have_fired {
                tracing::warn!(wakeup = %key, "the CLI's list does not name the wakeup ScheduleWakeup scheduled, which the CLI cannot have run yet; it stays until a list does");
                kept.insert(key, pending);
                continue;
            }
            // A `ScheduleWakeup` wakeup has fired when the CLI may have run it, and was
            // cancelled otherwise. Of the others the CLI does not say which: a one-shot wakeup
            // that fired and one `ScheduleWakeup {stop: true}` cancelled look the same.
            let fired = pending.due_at.is_none() || pending.may_have_fired;
            changes.extend(self.end_wakeup(&key, fired));
        }
        let mut pending_now = BTreeMap::new();
        for (key, mut cron) in listed {
            // The schedule the CLI wrote for people (`humanSchedule`) is kept over the bare
            // cron expression of a later list for the same wakeup.
            if !cron.human_schedule
                && let Some(known) = self
                    .tasks
                    .get(&key)
                    .and_then(|t| t.progress.as_ref())
                    .and_then(|p| p.summary.clone())
            {
                cron.schedule = Some(known);
                cron.human_schedule = true;
            }
            // What is known of it besides the list (the ScheduleWakeup's time and prompt).
            let pending = match self.live_crons.get(&key) {
                Some(known) => Pending {
                    cron: Cron {
                        prompt: known.cron.prompt.clone(),
                        ..cron
                    },
                    due_at: known.due_at,
                    may_have_fired: false,
                    unconfirmed: false,
                },
                None => Pending::listed(cron),
            };
            changes.extend(self.tasks.started(cron_info(&key, &pending)));
            pending_now.insert(key, pending);
        }
        pending_now.extend(kept);
        self.live_crons = pending_now;
        changes.extend(self.sync_live());
        merge_changes(changes)
    }

    /// The CLI started a command of its own (`command_lifecycle started` without `queued`: a
    /// cron trigger, a teammate shutdown prompt or a deferred-turn resume, per the CLI's
    /// schema; from a CLI that does not report command lifecycles, a run the adapter did not
    /// start): every one-shot wakeup known by then may have fired and left the CLI's list.
    ///
    /// The time a `ScheduleWakeup` result states (`scheduledFor`) is no bound: the CLI's
    /// scheduler runs a one-shot wakeup that lands on :00 or :30 up to 90 s before it ("one-shot
    /// tasks landing on :00 or :30 fire up to 90 s early", the CLI's `CronCreate` description;
    /// the window is a setting the CLI fetches, and 2.1.285 fires at the later of the stated
    /// time minus the window and the moment it created the wakeup, which the result does not
    /// give). So any command of the CLI's own may be the wakeup, whenever it starts.
    pub(crate) fn cli_command_started(&mut self) {
        for pending in self.live_crons.values_mut() {
            if !pending.cron.recurring {
                pending.may_have_fired = true;
            }
        }
    }

    /// A turn ended without the Stop hook (interrupted, or failed on an API error or a usage
    /// limit: the CLI sends the Stop hook, and with it its list of wakeups, only for a turn
    /// that ends normally). A one-shot wakeup that may have fired since the last list (the CLI
    /// started a command of its own since it became known, [`Self::cli_command_started`]) may
    /// have left the list without the adapter being told. Such wakeups are then no longer
    /// live (they do not keep the process, as a wakeup the list does not name never does) but
    /// stay as they are otherwise, until the next complete list: one that names them makes
    /// them live again, one that does not ends them like any wakeup that left the list.
    /// Recurring wakeups stay in the CLI's list when they fire, so they stay live; a wakeup
    /// created after the CLI's last command of its own cannot have fired.
    pub(crate) fn turn_ended_without_list(&mut self) -> Vec<BackgroundTaskInfo> {
        let mut dropped = Vec::new();
        for (key, pending) in self.live_crons.iter_mut() {
            if pending.cron.recurring || pending.unconfirmed {
                continue;
            }
            if pending.may_have_fired {
                pending.unconfirmed = true;
                dropped.push(key.clone());
            }
        }
        if dropped.is_empty() {
            return Vec::new();
        }
        tracing::debug!(wakeups = ?dropped, "a turn ended without the list of wakeups after the CLI may have run them; they wait for the next list");
        self.sync_live()
    }

    /// A `CronCreate` result (`{id, humanSchedule, recurring, durable}`) for the item
    /// `origin_item`.
    pub(crate) fn cron_created(
        &mut self,
        cron: Cron,
        origin_item: String,
    ) -> Vec<BackgroundTaskInfo> {
        let key = cron_key(&cron.id);
        let pending = Pending::listed(cron);
        let mut changes: Vec<BackgroundTaskInfo> = self
            .tasks
            .started(BackgroundTaskInfo {
                origin_item_key: Some(origin_item),
                ..cron_info(&key, &pending)
            })
            .into_iter()
            .collect();
        // Created just now: it is in the CLI's list, and cannot have fired.
        self.live_crons.insert(key, pending);
        changes.extend(self.sync_live());
        merge_changes(changes)
    }

    /// A `ScheduleWakeup` result of the tool use `tool_use_id` (its item `origin_item`). The
    /// wakeup of the dynamic loop the CLI had pending is gone (every call removes it): fired
    /// when the CLI may have run it (a command of the CLI's own started since it became
    /// known), cancelled otherwise. The call's own wakeup, when it scheduled one, is live from
    /// now on (`nextRunAt` = `scheduledFor`), until the CLI may have run it and a later list or
    /// signal says it is gone. Returns the changes and whether a wakeup was scheduled (the item
    /// goes on as it).
    pub(crate) fn wakeup_scheduled(
        &mut self,
        tool_use_id: &str,
        wakeup: Wakeup,
        origin_item: String,
    ) -> (Vec<BackgroundTaskInfo>, bool) {
        let mut changes = Vec::new();
        if let Some(key) = self.loop_wakeup.clone()
            && let Some(pending) = self.live_crons.get(&key)
        {
            let fired = pending.may_have_fired;
            changes.extend(self.end_wakeup(&key, fired));
        }
        let Some(due) = wakeup.scheduled_for else {
            changes.extend(self.sync_live());
            return (merge_changes(changes), false);
        };
        let key = format!("{WAKEUP_KEY_PREFIX}{tool_use_id}");
        let pending = Pending {
            cron: Cron {
                id: String::new(),
                prompt: wakeup.prompt,
                schedule: wakeup.reason,
                human_schedule: true,
                recurring: false,
            },
            due_at: Some(due),
            may_have_fired: false,
            unconfirmed: false,
        };
        changes.extend(self.tasks.started(BackgroundTaskInfo {
            origin_item_key: Some(origin_item),
            ..cron_info(&key, &pending)
        }));
        self.live_crons.insert(key.clone(), pending);
        self.loop_wakeup = Some(key);
        changes.extend(self.sync_live());
        (merge_changes(changes), true)
    }

    /// A `CronDelete` result (`{id}`): the wakeup was cancelled.
    pub(crate) fn cron_deleted(&mut self, id: &str) -> Vec<BackgroundTaskInfo> {
        let key = self.key_of_cron(id);
        let mut changes = self.end_wakeup(&key, false);
        changes.extend(self.sync_live());
        merge_changes(changes)
    }

    /// Tool uses a subagent made (its `assistant` message, `parent_tool_use_id` set). Only
    /// used to find the owners of tasks the subagent starts.
    pub(crate) fn subagent_tool_uses<'a>(
        &mut self,
        parent_tool_use_id: &str,
        tool_use_ids: impl IntoIterator<Item = &'a str>,
    ) -> Vec<BackgroundTaskInfo> {
        let mut changes = Vec::new();
        for id in tool_use_ids {
            self.sub_tools
                .insert(id.to_owned(), parent_tool_use_id.to_owned());
            let Some(task) = self.unresolved.get(id).cloned() else {
                continue;
            };
            let Some(owner) = self.owner_of(id) else {
                continue;
            };
            self.unresolved.remove(id);
            match self.hidden.get_mut(&task) {
                Some(hidden) => hidden.parent_key = Some(owner),
                None => changes.extend(self.tasks.update(&task, |t| {
                    t.parent_key.get_or_insert(owner);
                })),
            }
        }
        merge_changes(changes)
    }

    /// A subagent answered its tool use `tool_use_id`: a task it started has been reported
    /// before its result, so the tool use is no longer needed for finding owners.
    pub(crate) fn subagent_tool_result(&mut self, tool_use_id: &str) {
        self.sub_tools.remove(tool_use_id);
        self.unresolved.remove(tool_use_id);
    }

    /// Who asks in a `can_use_tool` request with `agent_id` (the id of a subagent task, or of
    /// an agent of a workflow as its `workflow_progress` names it).
    pub(crate) fn requester(&self, agent_id: &str) -> Requester {
        if self.tasks.get(agent_id).is_some() {
            return Requester::Task(agent_id.to_owned());
        }
        let workflow = self.meta.iter().find(|(_, m)| {
            m.workflow
                .values()
                .any(|w| w.agent_id.as_deref() == Some(agent_id))
        });
        if let Some((key, _)) = workflow
            && self.tasks.get(key).is_some()
        {
            return Requester::Task(key.clone());
        }
        if let Some(hidden) = self.hidden.get(agent_id) {
            // A subagent something waits for in the foreground: it belongs to the first shown
            // task up its owners (a background agent that started it), else to the turn.
            let mut seen = std::collections::HashSet::new();
            let mut owner = hidden.parent_key.clone();
            while let Some(key) = owner {
                if self.tasks.get(&key).is_some() {
                    return Requester::Task(key);
                }
                if !seen.insert(key.clone()) {
                    break;
                }
                owner = self.hidden.get(&key).and_then(|h| h.parent_key.clone());
            }
            return Requester::Foreground;
        }
        Requester::Unknown
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A moment of the tests (epoch ms), the base of the times their wakeups state
    /// (`scheduledFor`; the tracker only shows them).
    const NOW: i64 = 1_790_596_000_000;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    fn keys(changes: &[BackgroundTaskInfo]) -> Vec<&str> {
        changes.iter().map(|t| t.key.as_str()).collect()
    }

    /// Shape as Claude Code 2.1.283 sends it (claude-live E1).
    fn agent_started() -> Value {
        json!({"type": "system", "subtype": "task_started", "task_id": "a1", "tool_use_id": "toolu_A",
            "description": "Sleep then respond", "subagent_type": "general-purpose", "is_backgrounded": true,
            "spawn_depth": 1, "task_type": "local_agent", "prompt": "Run it"})
    }

    #[test]
    fn the_level_signal_may_come_before_the_start() {
        let mut t = Tracker::default();
        let changes = t.live_changed(&json!({"tasks": [
            {"task_id": "a1", "task_type": "local_agent", "description": "Sleep then respond"}]}));
        assert_eq!(keys(&changes), ["a1"]);
        let task = &changes[0];
        assert_eq!(
            (
                task.kind,
                task.title.as_str(),
                task.live,
                task.runs,
                task.stoppable
            ),
            (
                BackgroundTaskKind::Agent,
                "Sleep then respond",
                true,
                1,
                true
            )
        );
        // The start adds the launching item; the task is the same run.
        let changes = t.started(&agent_started(), Some("tool:toolu_A".into()));
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].origin_item_key.as_deref(), Some("tool:toolu_A"));
        assert_eq!((changes[0].runs, changes[0].live), (1, true));
        // Leaving the level set ends nothing: only the end edge does.
        let changes = t.live_changed(&json!({"tasks": []}));
        assert_eq!(
            (changes[0].live, changes[0].state),
            (false, BackgroundState::Running)
        );
    }

    #[test]
    fn a_start_before_the_level_is_not_live_until_listed() {
        let mut t = Tracker::default();
        let changes = t.started(&agent_started(), None);
        assert!(!changes[0].live, "busy comes from the live set only");
        let changes = t.live_changed(&json!({"tasks": [
            {"task_id": "a1", "task_type": "local_agent", "description": "x", "ambient": true}]}));
        assert_eq!((changes[0].live, changes[0].ambient), (true, true));
        assert!(!changes[0].keeps_busy(), "ambient work is not activity");
    }

    #[test]
    fn foreground_tasks_are_shown_only_once_they_are_background_work() {
        let mut t = Tracker::default();
        let fg = json!({"task_id": "b1", "owned_by_subagent": true, "tool_use_id": "toolu_B",
            "description": "ping", "is_backgrounded": false, "task_type": "local_bash"});
        assert!(t.started(&fg, None).is_empty());
        assert!(
            t.progress(&json!({"task_id": "b1", "description": "x",
                "usage": {"total_tokens": 1, "tool_uses": 1, "duration_ms": 1}}))
                .is_empty()
        );
        // Moved to the background by the CLI.
        let changes = t.updated(&json!({"task_id": "b1", "patch": {"is_backgrounded": true}}));
        assert_eq!(keys(&changes), ["b1"]);
        assert!(changes[0].progress.is_some(), "what was known comes along");
        // Another foreground task that simply ends is never shown.
        let fg2 = json!({"task_id": "b2", "tool_use_id": "toolu_C", "description": "sleep",
            "is_backgrounded": false, "task_type": "local_bash"});
        assert!(t.started(&fg2, None).is_empty());
        let (changes, known) = t.notification(
            &json!({"task_id": "b2", "status": "completed", "summary": "sleep", "output_file": ""}),
            None,
        );
        assert!(changes.is_empty() && known);
        // Listed in the live set: shown.
        let fg3 = json!({"task_id": "b3", "description": "x", "is_backgrounded": false,
            "task_type": "local_bash"});
        assert!(t.started(&fg3, None).is_empty());
        let changes = t.live_changed(
            &json!({"tasks": [{"task_id": "b3", "task_type": "local_bash", "description": "x"}]}),
        );
        assert!(keys(&changes).contains(&"b3"));
        // An end for a task never reported is not known.
        assert!(
            !t.notification(
                &json!({"task_id": "zz", "status": "completed", "summary": ""}),
                None
            )
            .1
        );
    }

    #[test]
    fn workflows_have_no_is_backgrounded_field_and_are_shown() {
        let mut t = Tracker::default();
        let wf = json!({"task_id": "w1", "tool_use_id": "toolu_W", "description": "Run two agents",
            "task_type": "local_workflow", "workflow_name": "simple"});
        let changes = t.started(&wf, None);
        assert_eq!(changes[0].kind, BackgroundTaskKind::Workflow);
    }

    #[test]
    fn a_restart_under_the_same_id_is_a_new_run() {
        let mut t = Tracker::default();
        t.started(&agent_started(), None);
        let changes =
            t.updated(&json!({"task_id": "a1", "patch": {"status": "completed", "end_time": 1}}));
        assert_eq!(changes[0].state, BackgroundState::Completed);
        let (changes, known) = t.notification(
            &json!({"task_id": "a1", "status": "completed",
            "summary": "done", "output_file": "x",
            "usage": {"total_tokens": 10, "tool_uses": 2, "duration_ms": 3}}),
            None,
        );
        assert!(known);
        assert_eq!(
            changes[0].result,
            Some(BackgroundOutcome {
                summary: Some("done".into()),
                exit_code: None,
                output: None,
                output_omitted_bytes: None,
            })
        );
        assert_eq!(changes[0].usage.unwrap().total_tokens, Some(10));
        let changes = t.started(&agent_started(), None);
        assert_eq!(
            (changes[0].runs, changes[0].state, changes[0].result.clone()),
            (2, BackgroundState::Running, None)
        );
    }

    #[test]
    fn updates_end_rename_and_ignore_states_that_are_not_ends() {
        let mut t = Tracker::default();
        t.started(&agent_started(), None);
        assert!(
            t.updated(&json!({"task_id": "a1", "patch": {"status": "paused"}}))
                .is_empty()
        );
        let changes = t.updated(&json!({"task_id": "a1", "patch": {"description": "renamed"}}));
        assert_eq!(changes[0].title, "renamed");
        let changes = t.updated(&json!({"task_id": "a1", "patch": {"status": "killed"}}));
        assert_eq!(changes[0].state, BackgroundState::Stopped);
        let (changes, _) = t.notification(
            &json!({"task_id": "a1", "status": "stopped", "summary": "s"}),
            None,
        );
        assert_eq!(changes[0].state, BackgroundState::Stopped);
        assert_eq!(
            changes[0].result.as_ref().unwrap().summary.as_deref(),
            Some("s")
        );
    }

    #[test]
    fn tasks_a_subagent_starts_belong_to_it() {
        // The subagent's tool use is seen before the task (the recorded order).
        let mut t = Tracker::default();
        t.started(&agent_started(), None);
        assert!(t.subagent_tool_uses("toolu_A", ["toolu_S"]).is_empty());
        let owned = json!({"task_id": "b1", "owned_by_subagent": true, "tool_use_id": "toolu_S",
            "description": "sleep", "is_backgrounded": true, "task_type": "local_bash"});
        let changes = t.started(&owned, None);
        assert_eq!(changes[0].parent_key.as_deref(), Some("a1"));
        t.subagent_tool_result("toolu_S");
        // The task before the tool use: the owner is filled in when the tool use is seen.
        let owned2 = json!({"task_id": "b2", "owned_by_subagent": true, "tool_use_id": "toolu_T",
            "description": "sleep", "is_backgrounded": true, "task_type": "local_bash"});
        assert_eq!(t.started(&owned2, None)[0].parent_key, None);
        let changes = t.subagent_tool_uses("toolu_A", ["toolu_T"]);
        assert_eq!(changes[0].parent_key.as_deref(), Some("a1"));
        // A subagent the background agent runs in the foreground: its requests belong to the
        // background agent, not to whatever turn runs.
        t.subagent_tool_uses("toolu_A", ["toolu_N"]);
        let nested = json!({"task_id": "n1", "tool_use_id": "toolu_N", "description": "nested",
            "is_backgrounded": false, "task_type": "local_agent"});
        assert!(t.started(&nested, None).is_empty());
        assert_eq!(t.requester("n1"), Requester::Task("a1".into()));
    }

    #[test]
    fn workflow_agents_merge_by_index_and_answer_for_their_requests() {
        let mut t = Tracker::default();
        let wf = json!({"task_id": "w1", "tool_use_id": "toolu_W", "description": "Two agents",
            "task_type": "local_workflow", "workflow_name": "simple"});
        t.started(&wf, None);
        // Recorded shapes (claude-live E3a).
        let changes = t.progress(&json!({"task_id": "w1", "description": "alpha",
            "summary": "Two agents", "last_tool_name": "alpha",
            "usage": {"total_tokens": 0, "tool_uses": 0, "duration_ms": 264},
            "workflow_progress": [
                {"type": "workflow_agent", "index": 1, "label": "alpha", "agentId": "ag1",
                 "model": "claude-haiku-4-5", "state": "start"},
                {"type": "workflow_agent", "index": 2, "label": "beta",
                 "model": "claude-haiku-4-5", "state": "start"},
                {"type": "workflow_phase", "index": 1, "title": "p"}]}));
        let progress = changes[0].progress.clone().unwrap();
        assert_eq!(progress.summary.as_deref(), Some("Two agents"));
        assert_eq!(progress.duration_ms, Some(264));
        assert_eq!(progress.workflow.len(), 2);
        // A later message with one agent updates that one only.
        let changes = t.progress(&json!({"task_id": "w1", "description": "beta",
            "usage": {"total_tokens": 5, "tool_uses": 0, "duration_ms": 900},
            "workflow_progress": [{"type": "workflow_agent", "index": 2, "label": "beta",
                "agentId": "ag2", "phaseTitle": "Build", "agentType": "Explore", "state": "done",
                "tokens": 5}]}));
        let agents = changes[0].progress.clone().unwrap().workflow;
        assert_eq!(agents[0].state, WorkflowAgentState::Start);
        assert_eq!(
            agents[1],
            WorkflowAgent {
                label: "beta".into(),
                phase: Some("Build".into()),
                state: WorkflowAgentState::Done,
                agent_type: Some("Explore".into()),
                model: None,
                tokens: Some(5)
            }
        );
        // A message without the list keeps it.
        let changes = t.progress(&json!({"task_id": "w1", "description": "x",
            "usage": {"total_tokens": 6, "tool_uses": 0, "duration_ms": 1}}));
        assert_eq!(changes[0].progress.clone().unwrap().workflow.len(), 2);
        assert_eq!(t.requester("ag2"), Requester::Task("w1".into()));
        assert_eq!(t.requester("w1"), Requester::Task("w1".into()));
        assert_eq!(t.requester("nobody"), Requester::Unknown);
        let fg_agent = json!({"task_id": "f1", "description": "fg", "is_backgrounded": false,
            "task_type": "local_agent"});
        t.started(&fg_agent, None);
        assert_eq!(t.requester("f1"), Requester::Foreground);
    }

    fn cron(id: &str, prompt: &str, schedule: &str, human: bool, recurring: bool) -> Cron {
        Cron {
            id: id.into(),
            prompt: prompt.into(),
            schedule: Some(schedule.into()),
            human_schedule: human,
            recurring,
        }
    }

    #[test]
    fn scheduled_wakeups_follow_the_pending_list() {
        let mut t = Tracker::default();
        let changes = t.cron_created(
            cron("c1", "tick", "Every minute", true, true),
            "tool:toolu_C".into(),
        );
        let task = &changes[0];
        assert_eq!(task.key, "cron:c1");
        assert_eq!(
            (task.kind, task.title.as_str(), task.live, task.stoppable),
            (BackgroundTaskKind::Scheduled, "tick", true, false)
        );
        assert_eq!(task.origin_item_key.as_deref(), Some("tool:toolu_C"));
        // The Stop hook's list (a bare cron expression) keeps the schedule written for people.
        let stop_hook =
            json!({"id": "c1", "schedule": "* * * * *", "recurring": true, "prompt": "tick"});
        assert!(
            t.crons_listed(vec![Cron::from_entry(&stop_hook).unwrap()])
                .is_empty()
        );
        assert_eq!(
            t.get("cron:c1")
                .unwrap()
                .progress
                .as_ref()
                .unwrap()
                .summary
                .as_deref(),
            Some("Every minute")
        );
        // The background tasks change; the wakeup stays live.
        t.live_changed(
            &json!({"tasks": [{"task_id": "b1", "task_type": "local_bash", "description": "x"}]}),
        );
        t.live_changed(&json!({"tasks": []}));
        assert!(t.get("cron:c1").unwrap().live);
        // A wakeup known only from the list (ScheduleWakeup gives no id).
        let changes = t.crons_listed(vec![
            Cron::from_entry(&stop_hook).unwrap(),
            Cron::from_entry(
                &json!({"id": "w9", "schedule": "18 21 * * *", "recurring": false,
                "prompt": "woke"}),
            )
            .unwrap(),
        ]);
        assert_eq!(keys(&changes), ["cron:w9"]);
        assert_eq!(changes[0].origin_item_key, None);
        // It came due: no longer listed.
        let changes = t.crons_listed(vec![Cron::from_entry(&stop_hook).unwrap()]);
        assert_eq!(
            (changes[0].key.as_str(), changes[0].state, changes[0].live),
            ("cron:w9", BackgroundState::Completed, false)
        );
        // CronDelete.
        let changes = t.cron_deleted("c1");
        assert_eq!(
            (changes[0].state, changes[0].live),
            (BackgroundState::Stopped, false)
        );
        assert!(t.crons_listed(Vec::new()).is_empty(), "already ended");
        assert!(t.iter().all(|task| !task.live));
        // CronList's job shape (recorded w4).
        let job = Cron::from_entry(&json!({"id": "j1", "cron": "* * * * *",
            "humanSchedule": "Every minute", "prompt": "Reply", "recurring": true,
            "durable": false}))
        .unwrap();
        assert_eq!(job, cron("j1", "Reply", "Every minute", true, true));
    }

    fn live_of(t: &Tracker, key: &str) -> (bool, BackgroundState) {
        let task = t.get(key).unwrap();
        (task.live, task.state)
    }

    /// Shapes of the Stop hook's `session_crons` (recorded w1, w4).
    fn listed(entries: &[(&str, bool)]) -> Vec<Cron> {
        entries
            .iter()
            .map(|(id, recurring)| {
                Cron::from_entry(&json!({"id": id, "schedule": "18 21 * * *",
                    "recurring": recurring, "prompt": format!("wake {id}")}))
                .unwrap()
            })
            .collect()
    }

    /// A one-shot wakeup leaves the CLI's list when it fires. After a turn that ended without
    /// the Stop hook while the CLI may have run a wakeup, the one-shot wakeups stop being live
    /// (they are not ended: whether they fired is not known) until the next list says.
    #[test]
    fn a_turn_without_the_list_leaves_one_shot_wakeups_unconfirmed() {
        let mut t = Tracker::default();
        t.crons_listed(listed(&[("r", true), ("w", false)]));
        let running = (true, BackgroundState::Running);
        assert_eq!(live_of(&t, "cron:w"), running);
        // The user's own turn was interrupted: nothing ran a wakeup, the list still holds.
        assert!(t.turn_ended_without_list().is_empty());
        assert_eq!(live_of(&t, "cron:w"), running);
        // The CLI started a command of its own (a wakeup came due) and that run failed.
        t.cli_command_started();
        let changes = t.turn_ended_without_list();
        assert_eq!(keys(&changes), ["cron:w"]);
        assert_eq!(live_of(&t, "cron:w"), (false, BackgroundState::Running));
        assert_eq!(
            live_of(&t, "cron:r"),
            running,
            "a recurring wakeup stays in the list when it fires"
        );
        // Once is enough: the next failed turn changes nothing more.
        assert!(t.turn_ended_without_list().is_empty());
        // The next list names it: it had not fired, and it is live again.
        let changes = t.crons_listed(listed(&[("r", true), ("w", false)]));
        assert_eq!(keys(&changes), ["cron:w"]);
        assert_eq!(live_of(&t, "cron:w"), running);
        assert_eq!(t.get("cron:w").unwrap().runs, 1);
        // Again, and this time the next list does not name it: it fired.
        t.cli_command_started();
        t.turn_ended_without_list();
        let changes = t.crons_listed(listed(&[("r", true)]));
        assert_eq!(keys(&changes), ["cron:w"]);
        assert_eq!(live_of(&t, "cron:w"), (false, BackgroundState::Completed));
        // A wakeup created after the CLI's command cannot be what it ran; one known before can.
        t.crons_listed(listed(&[("r", true), ("w2", false)]));
        t.cli_command_started();
        let changes = t.cron_created(
            cron("w3", "later", "in 1 hour", true, false),
            "tool:toolu_X".into(),
        );
        assert_eq!(
            (changes[0].key.as_str(), changes[0].live),
            ("cron:w3", true)
        );
        let changes = t.turn_ended_without_list();
        assert_eq!(keys(&changes), ["cron:w2"]);
        // One deleted ends.
        let changes = t.cron_deleted("w2");
        assert_eq!(
            (changes[0].state, changes[0].live),
            (BackgroundState::Stopped, false)
        );
        // Only confirmed wakeups keep the session busy.
        let busy: Vec<&str> = t
            .iter()
            .filter(|task| task.keeps_busy())
            .map(|task| task.key.as_str())
            .collect();
        assert_eq!(busy, ["cron:r", "cron:w3"]);
    }

    fn wakeup(at: i64, prompt: &str) -> Wakeup {
        Wakeup {
            scheduled_for: Some(at),
            prompt: prompt.into(),
            reason: Some("check later".into()),
        }
    }

    fn stop() -> Wakeup {
        Wakeup {
            scheduled_for: None,
            prompt: String::new(),
            reason: None,
        }
    }

    /// The list's entry for a wakeup (the shape of recording w1).
    fn entry(id: &str, prompt: &str) -> Cron {
        Cron::from_entry(
            &json!({"id": id, "schedule": "48 20 * * *", "recurring": false,
            "prompt": prompt}),
        )
        .unwrap()
    }

    /// `ScheduleWakeup`'s wakeup is live from its result, whatever ends the turn; the list names
    /// it under its own entry; any command the CLI starts of its own may be its run, and it
    /// ends when a list no longer holds it after that.
    #[test]
    fn a_schedule_wakeup_is_live_from_its_result_until_it_fired() {
        let mut t = Tracker::default();
        let due = NOW + 60_000;
        let (changes, scheduled) =
            t.wakeup_scheduled("toolu_W", wakeup(due, "woke"), "tool:toolu_W".into());
        assert!(scheduled);
        let key = "wakeup:toolu_W";
        assert_eq!(keys(&changes), [key]);
        let task = &changes[0];
        assert_eq!(
            (
                task.kind,
                task.title.as_str(),
                task.live,
                task.stoppable,
                task.next_run_at
            ),
            (
                BackgroundTaskKind::Scheduled,
                "woke",
                true,
                false,
                Some(due)
            )
        );
        assert_eq!(task.origin_item_key.as_deref(), Some("tool:toolu_W"));
        let running = (true, BackgroundState::Running);
        // The turn is interrupted: the wakeup is pending (nothing ran it).
        assert!(t.turn_ended_without_list().is_empty());
        assert_eq!(live_of(&t, key), running);
        // The list names it (a one-shot entry with its prompt): no task for the entry.
        let changes = t.crons_listed(vec![entry("180719e1", "woke")]);
        assert!(!keys(&changes).contains(&"cron:180719e1"), "{changes:?}");
        assert!(t.get("cron:180719e1").is_none());
        assert_eq!(live_of(&t, key), running);
        assert_eq!(t.get(key).unwrap().next_run_at, Some(due));
        // A command of the CLI's own (another cron, or this wakeup: the time is not compared),
        // whose run fails without a list: it may have run.
        t.cli_command_started();
        let changes = t.turn_ended_without_list();
        assert_eq!(keys(&changes), [key]);
        assert_eq!(live_of(&t, key), (false, BackgroundState::Running));
        // The next list names it: that command was not it, and it is live again.
        let changes = t.crons_listed(vec![entry("180719e1", "woke")]);
        assert_eq!(keys(&changes), [key]);
        assert_eq!(live_of(&t, key), running);
        assert_eq!(t.get(key).unwrap().runs, 1);
        // The next command of the CLI's own, and its run fails without a list.
        t.cli_command_started();
        assert_eq!(keys(&t.turn_ended_without_list()), [key]);
        // The next list does not name it: it fired.
        let changes = t.crons_listed(Vec::new());
        assert_eq!(keys(&changes), [key]);
        assert_eq!(live_of(&t, key), (false, BackgroundState::Completed));
    }

    /// The CLI's scheduler runs a one-shot wakeup that lands on :00 or :30 up to 90 s before the
    /// `scheduledFor` its `ScheduleWakeup` result stated. A command of the CLI's own that starts
    /// before that time is still the wakeup's possible run: the run that reschedules ends it as
    /// fired, a run that fails without a list leaves it unconfirmed, and a list that does not
    /// name it (never named before) ends it as fired.
    #[test]
    fn a_wakeup_the_cli_runs_before_its_scheduled_time_may_have_fired() {
        // 10:09:40 + 1200 s lands on 10:30:00; the CLI runs it at, say, 10:29:40. The tracker
        // gets no time at all: every signal below comes before `due`.
        let due = NOW + 20 * 60_000;
        let mut t = Tracker::default();
        t.wakeup_scheduled("toolu_1", wakeup(due, "tick"), "tool:1".into());
        t.crons_listed(vec![entry("c1", "tick")]);
        // Its run schedules the next wakeup: the one that ran completed (not cancelled).
        t.cli_command_started();
        let (changes, _) = t.wakeup_scheduled(
            "toolu_2",
            wakeup(due + 30 * 60_000, "tick"),
            "tool:2".into(),
        );
        assert_eq!(keys(&changes), ["wakeup:toolu_1", "wakeup:toolu_2"]);
        assert_eq!(
            live_of(&t, "wakeup:toolu_1"),
            (false, BackgroundState::Completed)
        );
        // The next one runs early too, and its run fails without a list: no longer trusted.
        t.cli_command_started();
        assert_eq!(keys(&t.turn_ended_without_list()), ["wakeup:toolu_2"]);
        assert_eq!(
            live_of(&t, "wakeup:toolu_2"),
            (false, BackgroundState::Running)
        );
        let changes = t.crons_listed(Vec::new());
        assert_eq!(keys(&changes), ["wakeup:toolu_2"]);
        assert_eq!(
            live_of(&t, "wakeup:toolu_2"),
            (false, BackgroundState::Completed)
        );
        assert!(t.iter().all(|task| !task.live));

        // Scheduled in a turn that ended without a list (no list ever named it); it runs early
        // and the list at the end of its run does not hold it: it fired, it is not kept.
        let mut t = Tracker::default();
        t.wakeup_scheduled("toolu_W", wakeup(due, "mine"), "tool:W".into());
        assert!(t.turn_ended_without_list().is_empty());
        t.cli_command_started();
        let changes = t.crons_listed(Vec::new());
        assert_eq!(keys(&changes), ["wakeup:toolu_W"]);
        assert_eq!(
            live_of(&t, "wakeup:toolu_W"),
            (false, BackgroundState::Completed)
        );
        // Its run failed instead: not trusted until the next list.
        let mut t = Tracker::default();
        t.wakeup_scheduled("toolu_W", wakeup(due, "mine"), "tool:W".into());
        t.cli_command_started();
        assert_eq!(keys(&t.turn_ended_without_list()), ["wakeup:toolu_W"]);
        assert!(!t.get("wakeup:toolu_W").unwrap().live);
    }

    /// Every `ScheduleWakeup` replaces the loop's pending wakeup (the CLI keeps one): one that
    /// may have fired completed, one the CLI cannot have run yet was cancelled. `stop: true`
    /// schedules nothing and ends the pending one; so does `CronDelete` of its entry.
    #[test]
    fn a_new_schedule_wakeup_replaces_the_pending_one() {
        let mut t = Tracker::default();
        t.wakeup_scheduled("toolu_1", wakeup(NOW + 60_000, "tick"), "tool:1".into());
        // Rescheduled before it came due.
        let (changes, _) =
            t.wakeup_scheduled("toolu_2", wakeup(NOW + 120_000, "tick"), "tool:2".into());
        assert_eq!(keys(&changes), ["wakeup:toolu_1", "wakeup:toolu_2"]);
        assert_eq!(
            live_of(&t, "wakeup:toolu_1"),
            (false, BackgroundState::Stopped)
        );
        // It came due and started the run that schedules the next one.
        t.cli_command_started();
        let (changes, _) =
            t.wakeup_scheduled("toolu_3", wakeup(NOW + 180_000, "tick"), "tool:3".into());
        assert_eq!(keys(&changes), ["wakeup:toolu_2", "wakeup:toolu_3"]);
        assert_eq!(
            live_of(&t, "wakeup:toolu_2"),
            (false, BackgroundState::Completed)
        );
        // `stop: true` before it came due.
        let (changes, scheduled) = t.wakeup_scheduled("toolu_4", stop(), "tool:4".into());
        assert!(!scheduled);
        assert_eq!(keys(&changes), ["wakeup:toolu_3"]);
        assert_eq!(
            live_of(&t, "wakeup:toolu_3"),
            (false, BackgroundState::Stopped)
        );
        assert!(t.get("wakeup:toolu_4").is_none());
        // CronDelete of the entry a list named.
        t.wakeup_scheduled("toolu_5", wakeup(NOW + 60_000, "tock"), "tool:5".into());
        t.crons_listed(vec![entry("c5", "tock")]);
        let changes = t.cron_deleted("c5");
        assert_eq!(keys(&changes), ["wakeup:toolu_5"]);
        assert_eq!(
            live_of(&t, "wakeup:toolu_5"),
            (false, BackgroundState::Stopped)
        );
        assert!(t.iter().all(|task| !task.live));
    }

    /// Which entry of the list is the `ScheduleWakeup` wakeup: a one-shot entry no task was
    /// reported for, with its prompt (a long prompt comes clipped). An entry that cannot be
    /// told apart, or none, leaves it as it is while the CLI cannot have run it.
    #[test]
    fn the_list_entry_of_a_schedule_wakeup_is_found_by_its_prompt() {
        let long = "x".repeat(1200);
        assert!(listed_prompt_matches("a", "a"));
        let clipped = format!("{}\u{2026} [+200 chars]", "x".repeat(1000));
        assert!(listed_prompt_matches(&clipped, &long));
        assert!(!listed_prompt_matches(
            &format!("{}\u{2026} [+199 chars]", "x".repeat(1000)),
            &long
        ));
        assert!(!listed_prompt_matches("b", "a"));
        assert!(!listed_prompt_matches("a [+1 chars]", "ab"));
        // UTF-16 units, as the CLI counts them.
        let emoji = "\u{1F600}".repeat(3);
        assert!(listed_prompt_matches(
            "\u{1F600}\u{2026} [+4 chars]",
            &emoji
        ));

        let mut t = Tracker::default();
        let due = NOW + 60_000;
        t.wakeup_scheduled("toolu_W", wakeup(due, &long), "tool:W".into());
        // A recurring entry, and a known cron, are not it; the clipped one-shot entry is.
        t.cron_created(
            cron("known", &long, "in 1 hour", true, false),
            "tool:K".into(),
        );
        let changes = t.crons_listed(vec![
            Cron::from_entry(
                &json!({"id": "every", "schedule": "* * * * *", "recurring": true,
                "prompt": clipped}),
            )
            .unwrap(),
            Cron {
                id: "known".into(),
                ..entry("known", &clipped)
            },
            entry("mine", &clipped),
        ]);
        assert!(!keys(&changes).contains(&"cron:mine"), "{changes:?}");
        assert_eq!(
            t.get("wakeup:toolu_W").unwrap().title,
            long,
            "its prompt, whole"
        );
        // Two entries it could be: it waits for a list that tells them apart.
        let mut t = Tracker::default();
        t.wakeup_scheduled("toolu_W", wakeup(due, "same"), "tool:W".into());
        t.crons_listed(vec![entry("a", "same"), entry("b", "same")]);
        assert_eq!(
            live_of(&t, "wakeup:toolu_W"),
            (true, BackgroundState::Running)
        );
        assert!(t.get("cron:a").is_some() && t.get("cron:b").is_some());
        // A list without any entry of it, while the CLI cannot have run it: it stays.
        let mut t = Tracker::default();
        t.wakeup_scheduled("toolu_W", wakeup(due, "mine"), "tool:W".into());
        t.crons_listed(vec![entry("other", "not mine")]);
        assert_eq!(
            live_of(&t, "wakeup:toolu_W"),
            (true, BackgroundState::Running)
        );
        // Scheduled in a turn without a list, it fired; the model did not reschedule and the CLI
        // armed the loop's next wakeup (same prompt) by itself: that entry is another wakeup.
        t.cli_command_started();
        let changes = t.crons_listed(vec![entry("other", "not mine"), entry("next", "mine")]);
        assert_eq!(keys(&changes), ["wakeup:toolu_W", "cron:next"]);
        assert_eq!(
            live_of(&t, "wakeup:toolu_W"),
            (false, BackgroundState::Completed)
        );
        assert_eq!(live_of(&t, "cron:next"), (true, BackgroundState::Running));
    }

    /// The end of a shell's output file is read when it is longer than the bound, from a
    /// character boundary; a short file whole.
    #[tokio::test]
    async fn output_files_are_read_whole_or_from_their_end() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("b1.output");
        std::fs::write(&path, "done-B\n\n[exited with code 0]\n").unwrap();
        assert_eq!(
            read_output_file(&path, 1024).await.unwrap(),
            OutputFile {
                text: "done-B\n\n[exited with code 0]\n".into(),
                omitted: None
            }
        );
        // "aé" + "z": the cut would fall inside "é" (2 bytes).
        std::fs::write(&path, "a\u{e9}z").unwrap();
        assert_eq!(
            read_output_file(&path, 2).await.unwrap(),
            OutputFile {
                text: "z".into(),
                omitted: Some(3)
            }
        );
        assert_eq!(
            read_output_file(&path, 3).await.unwrap(),
            OutputFile {
                text: "\u{e9}z".into(),
                omitted: Some(1)
            }
        );
        assert!(
            read_output_file(&dir.path().join("none"), 10)
                .await
                .is_err()
        );
    }

    /// Only a shown shell task's file is its output.
    #[test]
    fn the_output_file_is_read_for_shell_tasks_only() {
        let mut t = Tracker::default();
        t.started(
            &json!({"task_id": "b1", "tool_use_id": "toolu_B", "description": "make",
                "is_backgrounded": true, "task_type": "local_bash"}),
            None,
        );
        t.started(&agent_started(), None);
        t.started(
            &json!({"task_id": "f1", "description": "fg", "is_backgrounded": false,
                "task_type": "local_bash"}),
            None,
        );
        let end = |id: &str, file: &str| json!({"task_id": id, "status": "completed", "summary": "s", "output_file": file});
        assert_eq!(
            t.output_file_of(&end("b1", "C:\\t\\b1.output")),
            Some(std::path::PathBuf::from("C:\\t\\b1.output"))
        );
        assert_eq!(t.output_file_of(&end("b1", "")), None);
        assert_eq!(t.output_file_of(&end("a1", "C:\\t\\a1.output")), None);
        assert_eq!(t.output_file_of(&end("f1", "C:\\t\\f1.output")), None);
        assert_eq!(t.output_file_of(&end("zz", "C:\\t\\zz.output")), None);
        let (changes, _) = t.notification(
            &end("b1", "C:\\t\\b1.output"),
            Some(OutputFile {
                text: "log end\n".into(),
                omitted: Some(10),
            }),
        );
        let result = changes[0].result.clone().unwrap();
        assert_eq!(
            (result.output.as_deref(), result.output_omitted_bytes),
            (Some("log end\n"), Some(10))
        );
    }

    #[test]
    fn a_launch_names_the_item_that_goes_on_as_the_task() {
        let mut t = Tracker::default();
        // The result before any task message: the task comes from the result.
        let changes = t.launched(
            Launch {
                task_id: "b1".into(),
                kind: BackgroundTaskKind::Shell,
                title: "sleep".into(),
            },
            "tool:toolu_B".into(),
        );
        assert_eq!(changes[0].origin_item_key.as_deref(), Some("tool:toolu_B"));
        assert!(!changes[0].live, "not listed as live yet");
        // Already named by its start: nothing changes.
        t.started(&agent_started(), Some("tool:toolu_A".into()));
        assert!(
            t.launched(
                Launch {
                    task_id: "a1".into(),
                    kind: BackgroundTaskKind::Agent,
                    title: "x".into()
                },
                "tool:toolu_A".into()
            )
            .is_empty()
        );
    }
}
