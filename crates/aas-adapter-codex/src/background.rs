//! Codex work that runs outside the turn lifecycle (design.md §5.6, docs/adapters/codex.md §13):
//! terminals that a command leaves running when its turn ends, and sub-agent threads.
//!
//! Everything here comes from explicit app-server signals (codex-cli 0.148.0, recorded):
//!
//! * **Terminals.** Every process of Codex's unified exec starts with the `item/started` of a
//!   commandExecution item and ends with that item's `item/completed`, which Codex sends when
//!   the process exits, also after the turn ended (with the turn id of the turn that started
//!   it). `thread/backgroundTerminals/list` (experimental API) lists exactly the processes of a
//!   thread that have not exited: it is the level signal of the live terminals. A command item
//!   still open when its turn completes and listed there has become a background terminal; one
//!   that is not listed is not running (Codex never completes an item whose approval was still
//!   pending when the turn was interrupted).
//! * **Sub-agents.** A sub-agent is another thread of the same app-server. It is identified by
//!   the item that spawned it (`subAgentActivity{kind:"started"}` in v2,
//!   `collabAgentToolCall{tool:"spawnAgent"}` in v1) or by its `parentThreadId`
//!   (`thread/started`, `thread/read`). A run of it is one of its turns (`turn/started` to
//!   `turn/completed`); whether it is working is its `thread/status/changed` (`active`).
//!
//! This module is the pure bookkeeping; the session feeds it the notifications and the list
//! results and emits what it returns.

use std::collections::{HashMap, HashSet};

use aas_harness::{
    BackgroundOutcome, BackgroundProgress, BackgroundState, BackgroundTaskInfo, BackgroundTaskKind,
    BackgroundTasks, BackgroundUsage,
};

use crate::wire::WireBackgroundTerminal;

/// Where the messages of a thread belong.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Route {
    /// The session's own thread.
    Main,
    /// A sub-agent thread of this session (its thread id is its task key).
    Child(String),
    /// A thread of the app-server that is not part of this session's tree.
    Foreign,
    /// A thread not seen before.
    Unknown,
}

/// What `item/completed` of a commandExecution item means.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum CommandEnd {
    /// An item of the running turn: close it as usual.
    Item,
    /// The process of a background terminal ended: the task's new state. Its item was already
    /// closed as backgrounded.
    Terminal(Box<BackgroundTaskInfo>),
    /// The item's turn ended with the item open and its process not listed, so the engine closed
    /// it with the turn: nothing is left to report.
    Closed,
}

/// What the end of a turn changed.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct TurnEnd {
    /// Tasks whose state changed, to emit first.
    pub tasks: Vec<BackgroundTaskInfo>,
    /// Items of the main thread to close as backgrounded (their tasks are in `tasks`).
    pub backgrounded: Vec<String>,
}

#[derive(Debug, Default)]
struct Child {
    /// The running turn (what `turn/interrupt` needs).
    turn: Option<String>,
    /// The thread's total tokens (Codex's cumulative count) when the current run started.
    tokens_at_run_start: u64,
    /// The thread's latest total tokens.
    tokens: Option<u64>,
}

#[derive(Debug, Clone)]
struct Terminal {
    thread: String,
    process_id: String,
    /// Our terminate request for it stands (see [`Background::set_stop_requested`]).
    stop_requested: bool,
}

type ItemRef = (String, String);

/// The background work of one Codex session.
#[derive(Debug)]
pub(crate) struct Background {
    main: String,
    tasks: BackgroundTasks,
    children: HashMap<String, Child>,
    foreign: HashSet<String>,
    /// commandExecution items started and not completed: (thread, item) → the item's command.
    open: HashMap<ItemRef, String>,
    /// Items the last end of their thread's turn left open without a listed process.
    closed: HashSet<ItemRef>,
    /// Terminal tasks by key.
    terminals: HashMap<String, Terminal>,
    /// (thread, item) → terminal task key.
    terminal_of_item: HashMap<ItemRef, String>,
}

/// Key of the terminal task of `item`: the item id for the session's own thread (the task
/// names that item as its origin), qualified by the thread for a sub-agent's terminal.
fn terminal_key(main: &str, thread: &str, item: &str) -> String {
    if thread == main {
        item.to_owned()
    } else {
        format!("{thread}:{item}")
    }
}

impl Background {
    pub fn new(main: impl Into<String>) -> Self {
        Self {
            main: main.into(),
            tasks: BackgroundTasks::new(),
            children: HashMap::new(),
            foreign: HashSet::new(),
            open: HashMap::new(),
            closed: HashSet::new(),
            terminals: HashMap::new(),
            terminal_of_item: HashMap::new(),
        }
    }

    pub fn route(&self, thread: &str) -> Route {
        if thread == self.main {
            Route::Main
        } else if self.children.contains_key(thread) {
            Route::Child(thread.to_owned())
        } else if self.foreign.contains(thread) {
            Route::Foreign
        } else {
            Route::Unknown
        }
    }

    /// Whether `thread` is the session's thread or one of its sub-agents (a valid parent).
    pub fn in_tree(&self, thread: &str) -> bool {
        thread == self.main || self.children.contains_key(thread)
    }

    pub fn mark_foreign(&mut self, thread: &str) {
        self.foreign.insert(thread.to_owned());
    }

    pub fn task(&self, key: &str) -> Option<&BackgroundTaskInfo> {
        self.tasks.get(key)
    }

    // ----- sub-agents -----------------------------------------------------------------------

    /// Sub-agent thread `id` was spawned by `parent` (the session's thread or a known
    /// sub-agent). `origin_item` is the main thread's item that spawned it. A known sub-agent
    /// takes the origin and title of the spawning item when it had none (it was first seen
    /// through its own messages). Returns the task's new state when it changed.
    pub fn child_spawned(
        &mut self,
        id: &str,
        parent: &str,
        title: String,
        origin_item: Option<String>,
    ) -> Option<BackgroundTaskInfo> {
        if id == self.main {
            return None;
        }
        if self.children.contains_key(id) {
            return self.tasks.update(id, |task| {
                if task.origin_item_key.is_none() && origin_item.is_some() {
                    task.origin_item_key = origin_item;
                    task.title = title;
                }
            });
        }
        self.foreign.remove(id);
        self.children.insert(id.to_owned(), Child::default());
        let parent_key =
            (parent != self.main && self.children.contains_key(parent)).then(|| parent.to_owned());
        self.tasks.started(BackgroundTaskInfo {
            // Busy only once Codex reports the thread active.
            live: false,
            stoppable: true,
            origin_item_key: origin_item,
            parent_key,
            ..BackgroundTaskInfo::new(id, BackgroundTaskKind::Agent, title)
        })
    }

    /// `thread/status/changed` of a sub-agent: it works (or waits for an answer) while
    /// `active`.
    pub fn child_status(&mut self, id: &str, active: bool) -> Option<BackgroundTaskInfo> {
        self.tasks.update(id, |task| task.live = active)
    }

    /// `turn/started` of a sub-agent: a run begins. A sub-agent whose last run ended starts a
    /// new run (a follow-up message from its parent). A thread with a running turn is `active`
    /// in Codex's own terms, so the task is live even if that status has not arrived yet.
    pub fn child_turn_started(&mut self, id: &str, turn: &str) -> Option<BackgroundTaskInfo> {
        let child = self.children.get_mut(id)?;
        child.turn = Some(turn.to_owned());
        let known = self.tasks.get(id)?.clone();
        let restarted = if known.state.is_ended() {
            child.tokens_at_run_start = child.tokens.unwrap_or(0);
            self.tasks.started(BackgroundTaskInfo {
                state: BackgroundState::Running,
                progress: None,
                result: None,
                usage: None,
                ..known
            })
        } else {
            None
        };
        self.tasks.update(id, |task| task.live = true).or(restarted)
    }

    /// `turn/completed` of a sub-agent: its run ended with Codex's turn status and, as the
    /// summary, the run's final answer (or the turn's error). Without a running turn the thread
    /// is no longer `active` in Codex's own terms (its `idle` status comes with the end).
    pub fn child_turn_completed(
        &mut self,
        id: &str,
        status: &str,
        summary: Option<String>,
    ) -> Option<BackgroundTaskInfo> {
        let state = match status {
            "completed" => BackgroundState::Completed,
            "interrupted" => BackgroundState::Stopped,
            "inProgress" => return None,
            // "failed" and anything unknown.
            _ => BackgroundState::Failed,
        };
        let child = self.children.get_mut(id)?;
        child.turn = None;
        self.tasks.update(id, |task| {
            task.live = false;
            task.state = state;
            if let Some(summary) = summary {
                task.result = Some(BackgroundOutcome {
                    summary: Some(summary),
                    ..BackgroundOutcome::default()
                });
            }
        })
    }

    /// `thread/tokenUsage/updated` of a sub-agent (`total_tokens`: Codex's total of the
    /// thread). The task's usage is what its current run used.
    pub fn child_usage(&mut self, id: &str, total_tokens: u64) -> Option<BackgroundTaskInfo> {
        let child = self.children.get_mut(id)?;
        child.tokens = Some(total_tokens);
        let used = total_tokens.saturating_sub(child.tokens_at_run_start);
        self.tasks.update(id, |task| {
            let usage = task.usage.get_or_insert_with(BackgroundUsage::default);
            usage.total_tokens = Some(used);
        })
    }

    /// A sub-agent started a tool item (`tool`: what it runs): its run's progress.
    pub fn child_tool_started(&mut self, id: &str, tool: String) -> Option<BackgroundTaskInfo> {
        self.tasks.update(id, |task| {
            let progress = task
                .progress
                .get_or_insert_with(BackgroundProgress::default);
            progress.tool_uses = Some(progress.tool_uses.unwrap_or(0) + 1);
            progress.last_tool_name = Some(tool);
        })
    }

    /// Codex closed or deleted sub-agent thread `id`: nothing of it runs any more. A run that
    /// had not ended ends as stopped. Its terminals are no longer listed (a closed thread cannot
    /// be listed); their own end still comes with their `item/completed`.
    pub fn child_gone(&mut self, id: &str) -> Vec<BackgroundTaskInfo> {
        let Some(child) = self.children.get_mut(id) else {
            return Vec::new();
        };
        child.turn = None;
        let mut changed = Vec::new();
        let running = self.tasks.get(id).is_some_and(|t| !t.state.is_ended());
        let update = self.tasks.update(id, |task| {
            task.live = false;
            if running {
                task.state = BackgroundState::Stopped;
            }
        });
        changed.extend(update);
        let terminals: Vec<String> = self
            .terminals
            .iter()
            .filter(|(_, t)| t.thread == id)
            .map(|(key, _)| key.clone())
            .collect();
        for key in terminals {
            changed.extend(self.tasks.update(&key, |task| task.live = false));
        }
        changed
    }

    /// The running turn of sub-agent `key`: (thread, turn).
    pub fn child_turn(&self, key: &str) -> Option<(String, Option<String>)> {
        self.children
            .get(key)
            .map(|c| (key.to_owned(), c.turn.clone()))
    }

    // ----- terminals ------------------------------------------------------------------------

    /// `item/started` of a commandExecution item of `thread`.
    pub fn command_started(&mut self, thread: &str, item: &str, command: &str) {
        self.open
            .insert((thread.to_owned(), item.to_owned()), command.to_owned());
    }

    /// Whether `item` of `thread` is the item of a background terminal (its later output is
    /// not an item's any more).
    pub fn is_terminal_item(&self, thread: &str, item: &str) -> bool {
        self.terminal_of_item
            .contains_key(&(thread.to_owned(), item.to_owned()))
    }

    /// `item/completed` of a commandExecution item of `thread`, with the fields Codex reports.
    pub fn command_completed(
        &mut self,
        thread: &str,
        item: &str,
        status: Option<&str>,
        exit_code: Option<i32>,
        output: Option<String>,
        duration_ms: Option<u64>,
    ) -> CommandEnd {
        let at = (thread.to_owned(), item.to_owned());
        if self.open.remove(&at).is_some() {
            return CommandEnd::Item;
        }
        if let Some(key) = self.terminal_of_item.get(&at).cloned() {
            let stop_requested = self.terminals.get(&key).is_some_and(|t| t.stop_requested);
            let state = match status {
                Some("completed") => BackgroundState::Completed,
                // Codex ends a terminated process as failed (exit code -1).
                Some("failed") if stop_requested => BackgroundState::Stopped,
                // Not an end (a repeated start); nothing changes.
                Some("inProgress") => return CommandEnd::Closed,
                _ => BackgroundState::Failed,
            };
            let ended = self.tasks.get(&key).is_some_and(|t| t.state.is_ended());
            if ended {
                return CommandEnd::Closed;
            }
            return match self.tasks.update(&key, |task| {
                task.live = false;
                task.state = state;
                task.result = Some(BackgroundOutcome {
                    summary: None,
                    exit_code,
                    output,
                });
                if duration_ms.is_some() {
                    task.usage
                        .get_or_insert_with(BackgroundUsage::default)
                        .duration_ms = duration_ms;
                }
            }) {
                Some(task) => CommandEnd::Terminal(Box::new(task)),
                None => CommandEnd::Closed,
            };
        }
        if self.closed.remove(&at) {
            return CommandEnd::Closed;
        }
        CommandEnd::Item
    }

    /// Whether the end of a turn of `thread` needs Codex's terminal list: a command item of the
    /// thread is still open, or a terminal of the thread is live. Otherwise every terminal of
    /// the thread has reported its end and the list would be empty.
    pub fn needs_list(&self, thread: &str) -> bool {
        self.open.keys().any(|(t, _)| t == thread)
            || self.terminals.iter().any(|(key, t)| {
                t.thread == thread && self.tasks.get(key).is_some_and(|task| task.live)
            })
    }

    /// A turn of `thread` ended and Codex listed the thread's live terminals (`listed`): open
    /// command items whose process is listed become background terminals; open items whose
    /// process is not listed are closed with the turn by the engine.
    pub fn turn_ended(&mut self, thread: &str, listed: &[WireBackgroundTerminal]) -> TurnEnd {
        self.closed.retain(|(t, _)| t != thread);
        let mut end = TurnEnd::default();
        for entry in listed {
            let at = (thread.to_owned(), entry.item_id.clone());
            if let Some(command) = self.open.remove(&at) {
                let task = self.new_terminal(thread, entry, Some(&command));
                end.tasks.extend(task);
                if thread == self.main {
                    end.backgrounded.push(entry.item_id.clone());
                }
            } else if let Some(key) = self.terminal_of_item.get(&at).cloned() {
                end.tasks.extend(self.set_live(&key, true));
            } else {
                // A live process whose item this session never saw start: still Codex's work.
                end.tasks.extend(self.new_terminal(thread, entry, None));
            }
        }
        let left: Vec<ItemRef> = self
            .open
            .keys()
            .filter(|(t, _)| t == thread)
            .cloned()
            .collect();
        for at in left {
            self.open.remove(&at);
            self.closed.insert(at);
        }
        end.tasks.extend(self.unlisted_not_live(thread, listed));
        end
    }

    /// A turn of `thread` ended and its terminals could not be listed: open command items are
    /// left to the engine, which closes them with the turn.
    pub fn turn_ended_unlisted(&mut self, thread: &str) {
        self.closed.retain(|(t, _)| t != thread);
        let left: Vec<ItemRef> = self
            .open
            .keys()
            .filter(|(t, _)| t == thread)
            .cloned()
            .collect();
        for at in left {
            self.open.remove(&at);
            self.closed.insert(at);
        }
    }

    /// Codex listed the live terminals of `thread` again (after one of them ended). While a
    /// turn of the thread runs, processes of its open items are that turn's commands and
    /// processes this session never saw start are left for the turn's end.
    pub fn relisted(
        &mut self,
        thread: &str,
        listed: &[WireBackgroundTerminal],
        turn_running: bool,
    ) -> Vec<BackgroundTaskInfo> {
        let mut changed = Vec::new();
        for entry in listed {
            let at = (thread.to_owned(), entry.item_id.clone());
            if let Some(key) = self.terminal_of_item.get(&at).cloned() {
                changed.extend(self.set_live(&key, true));
            } else if !turn_running && !self.open.contains_key(&at) {
                changed.extend(self.new_terminal(thread, entry, None));
            }
        }
        changed.extend(self.unlisted_not_live(thread, listed));
        changed
    }

    /// The thread and Codex process number of terminal task `key`.
    pub fn terminal(&self, key: &str) -> Option<(String, String)> {
        self.terminals
            .get(key)
            .map(|t| (t.thread.clone(), t.process_id.clone()))
    }

    /// We ask Codex to terminate terminal `key` (`requested`), or Codex refused (`false`: it
    /// answered `terminated: false` or the request failed). Codex ends a terminated process as
    /// a failed command (exit code -1); a failed end while our request stands is the stop.
    /// The flag is set before the request is sent, because Codex may report the end before its
    /// answer to the request is read.
    pub fn set_stop_requested(&mut self, key: &str, requested: bool) {
        if let Some(terminal) = self.terminals.get_mut(key) {
            terminal.stop_requested = requested;
        }
    }

    /// A background terminal task for listed process `entry` of `thread`. `item_command` is the
    /// command of the open item that started it (`None`: a process this session never saw
    /// start, which has no item to name as its origin).
    fn new_terminal(
        &mut self,
        thread: &str,
        entry: &WireBackgroundTerminal,
        item_command: Option<&str>,
    ) -> Option<BackgroundTaskInfo> {
        let key = terminal_key(&self.main, thread, &entry.item_id);
        // The list's command is the one the agent wrote; the item's carries the shell wrapper.
        let title = match item_command {
            Some(command) if entry.command.is_empty() => command.to_owned(),
            _ => entry.command.clone(),
        };
        let main = thread == self.main;
        self.terminals.insert(
            key.clone(),
            Terminal {
                thread: thread.to_owned(),
                process_id: entry.process_id.clone(),
                stop_requested: false,
            },
        );
        self.terminal_of_item
            .insert((thread.to_owned(), entry.item_id.clone()), key.clone());
        let origin_item_key = (main && item_command.is_some()).then(|| entry.item_id.clone());
        self.tasks.started(BackgroundTaskInfo {
            stoppable: true,
            origin_item_key,
            parent_key: (!main).then(|| thread.to_owned()),
            ..BackgroundTaskInfo::new(key, BackgroundTaskKind::Shell, title)
        })
    }

    fn set_live(&mut self, key: &str, live: bool) -> Option<BackgroundTaskInfo> {
        let ended = self.tasks.get(key).is_some_and(|t| t.state.is_ended());
        if ended && live {
            return None;
        }
        self.tasks.update(key, |task| task.live = live)
    }

    /// Terminals of `thread` that are live but not in `listed` have exited (their
    /// `item/completed` follows).
    fn unlisted_not_live(
        &mut self,
        thread: &str,
        listed: &[WireBackgroundTerminal],
    ) -> Vec<BackgroundTaskInfo> {
        let gone: Vec<String> = self
            .terminal_of_item
            .iter()
            .filter(|((t, item), _)| t == thread && !listed.iter().any(|e| &e.item_id == item))
            .map(|(_, key)| key.clone())
            .collect();
        gone.iter()
            .filter_map(|key| self.set_live(key, false))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    const MAIN: &str = "main-thread";

    fn listed(entries: &[(&str, &str, &str)]) -> Vec<WireBackgroundTerminal> {
        entries
            .iter()
            .map(|(item, pid, command)| WireBackgroundTerminal {
                item_id: (*item).into(),
                process_id: (*pid).into(),
                command: (*command).into(),
            })
            .collect()
    }

    #[test]
    fn open_listed_commands_become_terminals_and_unlisted_ones_close_with_the_turn() {
        let mut bg = Background::new(MAIN);
        bg.command_started(MAIN, "a", "pwsh -Command 'a'");
        bg.command_started(MAIN, "b", "pwsh -Command 'b'");
        bg.command_started(MAIN, "pending", "pwsh -Command 'p'");
        assert!(bg.needs_list(MAIN));
        let end = bg.turn_ended(MAIN, &listed(&[("b", "3404", "b"), ("a", "88836", "a")]));
        assert_eq!(end.backgrounded, ["b", "a"]);
        assert_eq!(end.tasks.len(), 2);
        let a = bg.task("a").unwrap();
        assert_eq!(
            (a.kind, a.title.as_str(), a.live, a.state),
            (
                BackgroundTaskKind::Shell,
                "a",
                true,
                BackgroundState::Running
            )
        );
        assert_eq!(a.origin_item_key.as_deref(), Some("a"));
        assert!(a.stoppable);
        assert_eq!(bg.terminal("b"), Some((MAIN.to_owned(), "3404".to_owned())));
        // The item whose approval was pending never completes; if it does, it was closed.
        assert_eq!(
            bg.command_completed(MAIN, "pending", Some("completed"), Some(0), None, None),
            CommandEnd::Closed
        );
        // The terminal's output after the turn is not an item's any more.
        assert!(bg.is_terminal_item(MAIN, "a"));
    }

    #[test]
    fn a_terminal_ends_with_its_late_completion_and_a_terminate_makes_it_stopped() {
        let mut bg = Background::new(MAIN);
        bg.command_started(MAIN, "a", "a");
        bg.command_started(MAIN, "b", "b");
        bg.turn_ended(MAIN, &listed(&[("a", "1", "A"), ("b", "2", "B")]));
        bg.set_stop_requested("b", true);
        let CommandEnd::Terminal(b) = bg.command_completed(
            MAIN,
            "b",
            Some("failed"),
            Some(-1),
            Some("TICK\r\n".into()),
            Some(18029),
        ) else {
            panic!("terminal end expected")
        };
        assert_eq!((b.state, b.live), (BackgroundState::Stopped, false));
        assert_eq!(
            b.result,
            Some(BackgroundOutcome {
                summary: None,
                exit_code: Some(-1),
                output: Some("TICK\r\n".into())
            })
        );
        assert_eq!(b.usage.unwrap().duration_ms, Some(18029));
        let CommandEnd::Terminal(a) =
            bg.command_completed(MAIN, "a", Some("completed"), Some(0), None, None)
        else {
            panic!("terminal end expected")
        };
        assert_eq!(a.state, BackgroundState::Completed);
        // The first end wins.
        assert_eq!(
            bg.command_completed(MAIN, "a", Some("failed"), Some(1), None, None),
            CommandEnd::Closed
        );
        // A failed end without our stop is a failure.
        let mut bg = Background::new(MAIN);
        bg.command_started(MAIN, "c", "c");
        bg.turn_ended(MAIN, &listed(&[("c", "3", "C")]));
        let CommandEnd::Terminal(c) =
            bg.command_completed(MAIN, "c", Some("failed"), Some(2), None, None)
        else {
            panic!()
        };
        assert_eq!(c.state, BackgroundState::Failed);
        assert!(!bg.needs_list(MAIN));
    }

    #[test]
    fn relisting_follows_the_level_and_leaves_a_running_turns_commands_alone() {
        let mut bg = Background::new(MAIN);
        bg.command_started(MAIN, "a", "a");
        bg.command_started(MAIN, "b", "b");
        bg.turn_ended(MAIN, &listed(&[("a", "1", "A"), ("b", "2", "B")]));
        // A new turn runs a command of its own; `b` exited (its completion is on its way).
        bg.command_started(MAIN, "fg", "fg");
        let changed = bg.relisted(MAIN, &listed(&[("a", "1", "A"), ("fg", "5", "F")]), true);
        assert_eq!(changed.len(), 1);
        assert_eq!((changed[0].key.as_str(), changed[0].live), ("b", false));
        assert!(
            bg.task("fg").is_none(),
            "the running turn's command is not a task"
        );
        // A listed process nobody saw start is Codex's work too, once no turn runs.
        let changed = bg.relisted(MAIN, &listed(&[("a", "1", "A"), ("x", "9", "X")]), false);
        assert_eq!(changed.len(), 1);
        let x = &changed[0];
        assert_eq!((x.key.as_str(), x.title.as_str(), x.live), ("x", "X", true));
        assert_eq!(x.origin_item_key, None);
        // Only ended tasks never become live again.
        bg.command_completed(MAIN, "b", Some("completed"), Some(0), None, None);
        let changed = bg.relisted(MAIN, &listed(&[("b", "2", "B")]), false);
        assert!(
            changed.iter().all(|t| t.key != "b" && !t.live),
            "{changed:?}"
        );
        assert!(!bg.task("b").unwrap().live);
    }

    #[test]
    fn sub_agents_run_as_tasks_of_their_own_turns() {
        let mut bg = Background::new(MAIN);
        assert_eq!(bg.route("c1"), Route::Unknown);
        let c1 = bg
            .child_spawned("c1", MAIN, "/root/approver".into(), Some("spawn".into()))
            .unwrap();
        assert_eq!(bg.route("c1"), Route::Child("c1".into()));
        assert_eq!(
            (c1.kind, c1.live, c1.state, c1.parent_key.as_deref()),
            (
                BackgroundTaskKind::Agent,
                false,
                BackgroundState::Running,
                None
            )
        );
        assert_eq!(c1.origin_item_key.as_deref(), Some("spawn"));
        assert!(bg.child_status("c1", true).unwrap().live);
        assert_eq!(bg.child_turn_started("c1", "t1"), None, "the first run");
        assert_eq!(
            bg.child_turn("c1"),
            Some(("c1".to_owned(), Some("t1".to_owned())))
        );
        let t = bg
            .child_tool_started("c1", "commandExecution".into())
            .unwrap();
        assert_eq!(t.progress.unwrap().tool_uses, Some(1));
        assert_eq!(
            bg.child_usage("c1", 3030)
                .unwrap()
                .usage
                .unwrap()
                .total_tokens,
            Some(3030)
        );
        assert!(!bg.child_status("c1", false).unwrap().live);
        let done = bg
            .child_turn_completed("c1", "completed", Some("APPROVER_DONE".into()))
            .unwrap();
        assert_eq!(done.state, BackgroundState::Completed);
        assert_eq!(
            done.result.unwrap().summary.as_deref(),
            Some("APPROVER_DONE")
        );
        assert_eq!(bg.child_turn("c1"), Some(("c1".to_owned(), None)));
        // A follow-up from the parent is a new run; its usage counts from there.
        bg.child_status("c1", true);
        let again = bg.child_turn_started("c1", "t2").unwrap();
        assert_eq!((again.runs, again.state), (2, BackgroundState::Running));
        assert_eq!(
            (again.result, again.usage, again.progress),
            (None, None, None)
        );
        assert_eq!(
            bg.child_usage("c1", 4040)
                .unwrap()
                .usage
                .unwrap()
                .total_tokens,
            Some(1010)
        );
        let stopped = bg.child_turn_completed("c1", "interrupted", None).unwrap();
        assert_eq!(stopped.state, BackgroundState::Stopped);
    }

    #[test]
    fn grandchildren_name_their_parent_and_a_known_child_takes_its_spawning_item() {
        let mut bg = Background::new(MAIN);
        // Seen first through its own messages (resolved with thread/read).
        bg.child_spawned("c1", MAIN, "Sub-agent".into(), None);
        let c1 = bg
            .child_spawned("c1", MAIN, "/root/a".into(), Some("spawn".into()))
            .unwrap();
        assert_eq!(
            (c1.title.as_str(), c1.origin_item_key.as_deref()),
            ("/root/a", Some("spawn"))
        );
        let g = bg
            .child_spawned("g1", "c1", "/root/a/b".into(), None)
            .unwrap();
        assert_eq!(g.parent_key.as_deref(), Some("c1"));
        assert!(bg.in_tree("g1") && bg.in_tree(MAIN) && !bg.in_tree("x"));
        // A sub-agent's terminals are tasks under it (no item of the session).
        bg.command_started("c1", "sleep", "pwsh sleep");
        let end = bg.turn_ended("c1", &listed(&[("sleep", "86797", "Start-Sleep 300")]));
        assert!(end.backgrounded.is_empty());
        let t = &end.tasks[0];
        assert_eq!(t.key, "c1:sleep");
        assert_eq!(t.parent_key.as_deref(), Some("c1"));
        assert_eq!(t.origin_item_key, None);
        // The thread is closed: its run is over and nothing of it is listed any more.
        bg.child_status("c1", true);
        let gone = bg.child_gone("c1");
        assert_eq!(gone.len(), 2);
        assert!(gone.iter().all(|t| !t.live));
        assert_eq!(bg.task("c1").unwrap().state, BackgroundState::Stopped);
        assert_eq!(bg.task("c1:sleep").unwrap().state, BackgroundState::Running);
        bg.mark_foreign("f");
        assert_eq!(bg.route("f"), Route::Foreign);
    }

    #[test]
    fn an_unlisted_turn_end_leaves_the_items_to_the_engine() {
        let mut bg = Background::new(MAIN);
        bg.command_started(MAIN, "a", "a");
        bg.turn_ended_unlisted(MAIN);
        assert!(!bg.needs_list(MAIN));
        assert_eq!(
            bg.command_completed(MAIN, "a", Some("completed"), Some(0), None, None),
            CommandEnd::Closed
        );
        assert!(bg.task("a").is_none());
    }
}
