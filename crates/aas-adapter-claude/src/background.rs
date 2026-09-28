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
//!   results name single wakeups. A wakeup is live while that list holds it — and only while
//!   the list is known: a one-shot wakeup leaves the CLI's list when it fires, so after a turn
//!   that ended without the Stop hook while the CLI may have run a wakeup, the one-shot
//!   wakeups are no longer live until the next complete list names them
//!   ([`Tracker::turn_ended_without_list`]).
//!
//! The tracker never ends a task because of time and never reads text written for people.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use aas_harness::{
    BackgroundOutcome, BackgroundProgress, BackgroundState, BackgroundTaskInfo, BackgroundTaskKind,
    BackgroundTasks, BackgroundUsage, LiveEntry, WorkflowAgent, WorkflowAgentState,
};
use serde_json::Value;

/// Prefix of the keys of scheduled wakeups (`cron:<id>`): the CLI's cron ids and task ids are
/// separate namespaces.
pub(crate) const CRON_KEY_PREFIX: &str = "cron:";

/// The task key of the scheduled wakeup `id`.
pub(crate) fn cron_key(id: &str) -> String {
    format!("{CRON_KEY_PREFIX}{id}")
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
    live_crons: BTreeMap<String, Cron>,
    /// Wakeups of `live_crons` whose place in the CLI's list is not known any more (one-shot
    /// wakeups after a turn that ended without the Stop hook): shown, but not live, until the
    /// next complete list says whether they are still pending.
    unconfirmed_crons: BTreeSet<String>,
    /// The CLI started a command of its own (`command_lifecycle started` without `queued`: a
    /// cron trigger, a teammate shutdown prompt or a deferred-turn resume, per the CLI's
    /// schema) since its last complete list of wakeups: a one-shot wakeup may have fired and
    /// left that list.
    cli_command_since_list: bool,
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

fn cron_info(cron: &Cron) -> BackgroundTaskInfo {
    BackgroundTaskInfo {
        // Claude Code has no request that cancels a single wakeup.
        stoppable: false,
        progress: cron.schedule.as_ref().map(|s| BackgroundProgress {
            summary: Some(s.clone()),
            ..BackgroundProgress::default()
        }),
        ..BackgroundTaskInfo::new(
            cron_key(&cron.id),
            BackgroundTaskKind::Scheduled,
            cron.prompt.clone(),
        )
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
            unconfirmed_crons,
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
                    .keys()
                    .filter(|key| !unconfirmed_crons.contains(*key))
                    .map(|key| LiveEntry {
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
                Some(cron) => cron_info(cron),
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

    /// `system/task_notification {status: completed|failed|stopped, summary, usage?}`: the
    /// end of a run. Returns the changes and whether the task was known.
    pub(crate) fn notification(&mut self, msg: &Value) -> (Vec<BackgroundTaskInfo>, bool) {
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
                t.result = Some(BackgroundOutcome {
                    summary,
                    exit_code: None,
                    output: None,
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

    /// The complete list of pending wakeups (Stop hook `session_crons`, `CronList` result).
    /// A listed wakeup is live; one that is no longer listed is not pending any more.
    pub(crate) fn crons_listed(&mut self, crons: Vec<Cron>) -> Vec<BackgroundTaskInfo> {
        let listed: BTreeMap<String, Cron> =
            crons.into_iter().map(|c| (cron_key(&c.id), c)).collect();
        let mut changes = Vec::new();
        let gone: Vec<String> = self
            .live_crons
            .keys()
            .filter(|k| !listed.contains_key(*k))
            .cloned()
            .collect();
        for key in gone {
            // Fired (a one-shot wakeup leaves the list once it ran) or cancelled by
            // `ScheduleWakeup {stop: true}`: the CLI does not say which.
            changes.extend(self.tasks.update(&key, |t| {
                if !t.state.is_ended() {
                    t.state = BackgroundState::Completed;
                }
            }));
        }
        for (key, mut cron) in listed.clone() {
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
            }
            changes.extend(self.tasks.started(cron_info(&cron)));
        }
        self.live_crons = listed;
        // The list is known again: what it names is pending, and nothing may have fired since.
        self.unconfirmed_crons.clear();
        self.cli_command_since_list = false;
        changes.extend(self.sync_live());
        merge_changes(changes)
    }

    /// The CLI started a command of its own (see `cli_command_since_list`).
    pub(crate) fn cli_command_started(&mut self) {
        self.cli_command_since_list = true;
    }

    /// A turn ended without the Stop hook (interrupted, or failed on an API error or a usage
    /// limit: the CLI sends the Stop hook, and with it its list of wakeups, only for a turn
    /// that ends normally). When the CLI may have run a wakeup since its last complete list —
    /// it started a command of its own, or it does not report command lifecycles at all
    /// (`lifecycle_reported: false`) — a one-shot wakeup may have fired and left the list
    /// without the adapter being told. The one-shot wakeups are then no longer live (they do
    /// not keep the process, as a wakeup the list does not name never does) but stay as they
    /// are otherwise, until the next complete list: one that names them makes them live again,
    /// one that does not ends them like any wakeup that left the list. Recurring wakeups stay
    /// in the CLI's list when they fire, so they stay live.
    pub(crate) fn turn_ended_without_list(
        &mut self,
        lifecycle_reported: bool,
    ) -> Vec<BackgroundTaskInfo> {
        let may_have_fired = self.cli_command_since_list || !lifecycle_reported;
        self.cli_command_since_list = false;
        if !may_have_fired {
            return Vec::new();
        }
        let one_shot: Vec<String> = self
            .live_crons
            .iter()
            .filter(|(_, cron)| !cron.recurring)
            .map(|(key, _)| key.clone())
            .collect();
        if one_shot.is_empty() {
            return Vec::new();
        }
        tracing::debug!(wakeups = ?one_shot, "a turn ended without the list of wakeups after the CLI may have run one; the one-shot wakeups wait for the next list");
        self.unconfirmed_crons.extend(one_shot);
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
        let mut changes: Vec<BackgroundTaskInfo> = self
            .tasks
            .started(BackgroundTaskInfo {
                origin_item_key: Some(origin_item),
                ..cron_info(&cron)
            })
            .into_iter()
            .collect();
        // Created just now: it is in the CLI's list.
        self.unconfirmed_crons.remove(&key);
        self.live_crons.insert(key, cron);
        changes.extend(self.sync_live());
        merge_changes(changes)
    }

    /// A `CronDelete` result (`{id}`): the wakeup was cancelled.
    pub(crate) fn cron_deleted(&mut self, id: &str) -> Vec<BackgroundTaskInfo> {
        let key = cron_key(id);
        self.live_crons.remove(&key);
        self.unconfirmed_crons.remove(&key);
        let mut changes: Vec<BackgroundTaskInfo> = self
            .tasks
            .update(&key, |t| {
                if !t.state.is_ended() {
                    t.state = BackgroundState::Stopped;
                }
            })
            .into_iter()
            .collect();
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
            !t.notification(&json!({"task_id": "zz", "status": "completed", "summary": ""}))
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
        let (changes, known) = t.notification(&json!({"task_id": "a1", "status": "completed",
            "summary": "done", "output_file": "x",
            "usage": {"total_tokens": 10, "tool_uses": 2, "duration_ms": 3}}));
        assert!(known);
        assert_eq!(
            changes[0].result,
            Some(BackgroundOutcome {
                summary: Some("done".into()),
                exit_code: None,
                output: None
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
        let (changes, _) =
            t.notification(&json!({"task_id": "a1", "status": "stopped", "summary": "s"}));
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
        assert!(t.turn_ended_without_list(true).is_empty());
        assert_eq!(live_of(&t, "cron:w"), running);
        // The CLI started a command of its own (a wakeup came due) and that run failed.
        t.cli_command_started();
        let changes = t.turn_ended_without_list(true);
        assert_eq!(keys(&changes), ["cron:w"]);
        assert_eq!(live_of(&t, "cron:w"), (false, BackgroundState::Running));
        assert_eq!(
            live_of(&t, "cron:r"),
            running,
            "a recurring wakeup stays in the list when it fires"
        );
        // Once is enough: the next failed turn changes nothing more.
        assert!(t.turn_ended_without_list(true).is_empty());
        // The next list names it: it had not fired, and it is live again.
        let changes = t.crons_listed(listed(&[("r", true), ("w", false)]));
        assert_eq!(keys(&changes), ["cron:w"]);
        assert_eq!(live_of(&t, "cron:w"), running);
        assert_eq!(t.get("cron:w").unwrap().runs, 1);
        // Again, and this time the next list does not name it: it fired.
        t.cli_command_started();
        t.turn_ended_without_list(true);
        let changes = t.crons_listed(listed(&[("r", true)]));
        assert_eq!(keys(&changes), ["cron:w"]);
        assert_eq!(live_of(&t, "cron:w"), (false, BackgroundState::Completed));
        // A CLI that does not report command lifecycles may have run one in any turn.
        t.crons_listed(listed(&[("r", true), ("w2", false)]));
        let changes = t.turn_ended_without_list(false);
        assert_eq!(keys(&changes), ["cron:w2"]);
        // A wakeup created after that is in the list; one deleted ends.
        let changes = t.cron_created(
            cron("w3", "later", "in 1 hour", true, false),
            "tool:toolu_X".into(),
        );
        assert_eq!(
            (changes[0].key.as_str(), changes[0].live),
            ("cron:w3", true)
        );
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
