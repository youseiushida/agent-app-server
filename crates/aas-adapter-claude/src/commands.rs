//! Claude Code's slash commands for the composer's `/` menu (docs/adapters/claude.md §8).
//!
//! The CLI lists its commands with their descriptions and aliases in `initialize.commands` and,
//! whenever the list changes (skills or MCP prompts added), in `system/commands_changed`. Every
//! turn's `system/init` names the commands again (`slash_commands`, canonical names only) and the
//! ones that only work in a terminal (`terminal_slash_commands`). The menu is the full list with
//! every alias as a command of its own (Claude Code resolves the alias), without the terminal-only
//! commands and without the names of [`HIDDEN_COMMANDS`]; a name `system/init` adds that the full
//! list does not know is offered without a description.
//!
//! Everything here is keyed by the names the CLI reports; nothing is read from descriptions.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use aas_harness::protocol::{Command, CommandAction, CommandSource};
use parking_lot::Mutex;
use serde_json::Value;

/// Commands of Claude Code 2.1.284 that are never offered, with the reason (the CLI marks none
/// of them in its list, so they are named here; revisit with every CLI update, §8):
pub(crate) const HIDDEN_COMMANDS: &[(&str, &str)] = &[
    (
        "__remote-workflow",
        "runs a workflow script delivered by the server; server-launched sessions only",
    ),
    (
        "workflow-launch-exec",
        "executes a server-launched workflow handoff; workflow_launch event sessions only",
    ),
    ("extra-usage", "a stub: \"Renamed to /usage-credits\""),
    (
        "agents",
        "a stub: \"(removed) Ask Claude to create/manage subagents\"",
    ),
    (
        "heapdump",
        "dumps the CLI process's JS heap to the desktop of the machine; a diagnostic of the process, not of the conversation",
    ),
    (
        "design-consent",
        "the same action as `/design consent`, which the list offers",
    ),
    (
        "design-revoke",
        "the same action as `/design revoke`, which the list offers",
    ),
];

/// Claude Code's commands that switch the native session inside the running process (see
/// [`aas_harness::HarnessAdapter::session_switching_commands`]):
/// * `clear` — "Start a new session with empty context; previous session stays on disk
///   (resumable with /resume)"; it runs in stream-json mode and the CLI reports a new
///   `session_id`;
/// * `resume` — its session picker (interactive only in 2.1.284; refused should a version run it
///   in this mode).
pub(crate) const SESSION_SWITCHING_COMMANDS: &[&str] = &["clear", "resume"];

/// The aliases of [`SESSION_SWITCHING_COMMANDS`] in Claude Code 2.1.284, for the time before
/// the CLI listed them: `clear` has `reset` and `new` (listed in `initialize.commands`),
/// `resume` has `continue` (its interactive command definition; `resume` is not listed in
/// stream-json mode). Aliases a later CLI lists are learned from its list as well.
pub(crate) const SESSION_SWITCHING_ALIASES: &[&str] = &["reset", "new", "continue"];

/// A command as Claude Code lists it (`initialize.commands`, `system/commands_changed`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ListedCommand {
    pub name: String,
    pub description: Option<String>,
    pub argument_hint: Option<String>,
    pub aliases: Vec<String>,
}

fn str_field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

fn names_of(list: Option<&Value>) -> Vec<String> {
    list.and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(|n| n.trim_start_matches('/').to_owned())
        .filter(|n| !n.is_empty())
        .collect()
}

/// The entries of a command list (`[{name, description, argumentHint, aliases?, builtin?}]`).
pub(crate) fn listed_commands(list: Option<&Value>) -> Vec<ListedCommand> {
    list.and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|c| {
            let name = str_field(c, "name")?.trim_start_matches('/').to_owned();
            if name.is_empty() {
                return None;
            }
            Some(ListedCommand {
                description: str_field(c, "description")
                    .filter(|d| !d.is_empty())
                    .map(str::to_owned),
                argument_hint: str_field(c, "argumentHint")
                    .filter(|d| !d.is_empty())
                    .map(str::to_owned),
                aliases: names_of(c.get("aliases")),
                name,
            })
        })
        .collect()
}

fn is_hidden(name: &str) -> bool {
    HIDDEN_COMMANDS.iter().any(|(hidden, _)| *hidden == name)
}

fn command(name: &str, description: Option<String>, argument_hint: Option<String>) -> Command {
    Command {
        name: name.to_owned(),
        description,
        source: CommandSource::Harness,
        argument_hint,
        action: CommandAction::InsertText {
            text: format!("/{name} "),
        },
    }
}

/// What the CLI said about its commands, and the menu made from it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct CommandView {
    /// The last full list (`initialize.commands` or `system/commands_changed`).
    pub listed: Vec<ListedCommand>,
    /// The last `system/init.slash_commands`.
    pub init_names: Vec<String>,
    /// The commands that only work in a terminal: the last `system/init.terminal_slash_commands`
    /// (or the one remembered for the working directory, before the first turn).
    pub terminal: Vec<String>,
}

impl CommandView {
    /// Takes a full list from `initialize.commands` or `system/commands_changed.commands`.
    pub(crate) fn set_listed(&mut self, list: Option<&Value>) {
        self.listed = listed_commands(list);
    }

    /// Takes the names of a `system/init`.
    pub(crate) fn set_init(&mut self, init: &Value) {
        if init.get("slash_commands").is_some() {
            self.init_names = names_of(init.get("slash_commands"));
        }
        if init.get("terminal_slash_commands").is_some() {
            self.terminal = names_of(init.get("terminal_slash_commands"));
        }
    }

    /// The menu: every listed command and alias, then the names `system/init` added, without
    /// the terminal-only and hidden commands, each name once.
    pub(crate) fn visible(&self) -> Vec<Command> {
        let offered = |name: &str| !is_hidden(name) && !self.terminal.iter().any(|t| t == name);
        let mut seen: BTreeSet<String> = BTreeSet::new();
        let mut out = Vec::new();
        for c in &self.listed {
            if offered(&c.name) && seen.insert(c.name.clone()) {
                out.push(command(
                    &c.name,
                    c.description.clone(),
                    c.argument_hint.clone(),
                ));
            }
        }
        // Aliases after the names, so that a name is never taken by another command's alias.
        for c in &self.listed {
            if !offered(&c.name) {
                continue;
            }
            for alias in &c.aliases {
                if offered(alias) && seen.insert(alias.clone()) {
                    out.push(command(
                        alias,
                        c.description.clone(),
                        c.argument_hint.clone(),
                    ));
                }
            }
        }
        for name in &self.init_names {
            if offered(name) && seen.insert(name.clone()) {
                out.push(command(name, None, None));
            }
        }
        out
    }

    /// Aliases the list gives the session-switching commands.
    pub(crate) fn switching_aliases(&self) -> impl Iterator<Item = &str> {
        self.listed
            .iter()
            .filter(|c| SESSION_SWITCHING_COMMANDS.contains(&c.name.as_str()))
            .flat_map(|c| c.aliases.iter().map(String::as_str))
    }
}

/// What the adapter and its sessions share about commands.
#[derive(Debug, Default)]
pub(crate) struct SharedCommands {
    /// The menu per working directory.
    by_cwd: HashMap<PathBuf, Vec<Command>>,
    /// The terminal-only commands the last `system/init` in a working directory named.
    terminal_by_cwd: HashMap<PathBuf, Vec<String>>,
    /// The terminal-only commands the last `system/init` anywhere named: the CLI's own list,
    /// for a directory where no turn ran yet.
    latest_terminal: Option<Vec<String>>,
    /// Aliases of the session-switching commands the CLI listed.
    switching_aliases: BTreeSet<String>,
}

pub(crate) type CommandCache = Arc<Mutex<SharedCommands>>;

impl SharedCommands {
    pub(crate) fn menu(&self, cwd: &Path) -> Option<Vec<Command>> {
        self.by_cwd.get(cwd).cloned()
    }

    pub(crate) fn set_menu(&mut self, cwd: &Path, menu: Vec<Command>) {
        self.by_cwd.insert(cwd.to_path_buf(), menu);
    }

    /// The terminal-only commands to leave out in `cwd` before its first turn.
    pub(crate) fn terminal_for(&self, cwd: &Path) -> Vec<String> {
        self.terminal_by_cwd
            .get(cwd)
            .or(self.latest_terminal.as_ref())
            .cloned()
            .unwrap_or_default()
    }

    pub(crate) fn remember_terminal(&mut self, cwd: &Path, terminal: &[String]) {
        self.terminal_by_cwd
            .insert(cwd.to_path_buf(), terminal.to_vec());
        self.latest_terminal = Some(terminal.to_vec());
    }

    pub(crate) fn learn(&mut self, view: &CommandView) {
        self.switching_aliases
            .extend(view.switching_aliases().map(str::to_owned));
    }

    /// Every name under which Claude Code runs a session-switching command.
    pub(crate) fn switching_names(&self) -> Vec<String> {
        let mut names: Vec<String> = SESSION_SWITCHING_COMMANDS
            .iter()
            .chain(SESSION_SWITCHING_ALIASES)
            .map(|n| (*n).to_owned())
            .collect();
        for alias in &self.switching_aliases {
            if !names.contains(alias) {
                names.push(alias.clone());
            }
        }
        names
    }
}

/// The menu of an `initialize` response in `cwd`, before any turn there (the terminal-only
/// commands remembered for it), and what it teaches about aliases.
pub(crate) fn menu_from_initialize(cache: &CommandCache, cwd: &Path, init: &Value) -> Vec<Command> {
    let mut cache = cache.lock();
    let mut view = CommandView {
        terminal: cache.terminal_for(cwd),
        ..CommandView::default()
    };
    view.set_listed(init.get("commands"));
    cache.learn(&view);
    let menu = view.visible();
    cache.set_menu(cwd, menu.clone());
    menu
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    /// Entries in the shape of Claude Code 2.1.284's `initialize.commands` (descriptions as
    /// reported, shortened).
    fn listed() -> Value {
        json!([
            {"name": "clear", "description": "Start a new session with empty context; previous session stays on disk (resumable with /resume)", "argumentHint": "[name]", "aliases": ["reset", "new"], "builtin": true},
            {"name": "compact", "description": "Free up context by summarizing the conversation so far", "argumentHint": "<optional custom summarization instructions>", "builtin": true},
            {"name": "code-review", "description": "Review the current diff", "argumentHint": "[low|medium|high]", "aliases": ["review"], "builtin": true},
            {"name": "color", "description": "Set the prompt bar color for this session", "argumentHint": "", "builtin": true},
            {"name": "heapdump", "description": "Dump the JS heap to the Desktop", "argumentHint": "", "builtin": true},
            {"name": "docs", "description": "docs skill", "argumentHint": "", "aliases": ["anthropic-skills:docs"]},
            {"name": "rename", "description": "Rename the current conversation", "argumentHint": "[name]", "aliases": ["name"], "builtin": true},
            {"name": ""}
        ])
    }

    fn names(menu: &[Command]) -> Vec<&str> {
        menu.iter().map(|c| c.name.as_str()).collect()
    }

    #[test]
    fn the_menu_expands_aliases_and_leaves_out_terminal_and_hidden_commands() {
        let mut view = CommandView::default();
        view.set_listed(Some(&listed()));
        // Before the first turn nothing is known about terminal-only commands.
        assert_eq!(
            names(&view.visible()),
            [
                "clear",
                "compact",
                "code-review",
                "color",
                "docs",
                "rename",
                "reset",
                "new",
                "review",
                "anthropic-skills:docs",
                "name"
            ]
        );
        // The alias inserts itself and carries its command's description and hint.
        let review = view
            .visible()
            .into_iter()
            .find(|c| c.name == "review")
            .unwrap();
        assert_eq!(
            review.description.as_deref(),
            Some("Review the current diff")
        );
        assert_eq!(review.argument_hint.as_deref(), Some("[low|medium|high]"));
        assert_eq!(
            review.action,
            CommandAction::InsertText {
                text: "/review ".into()
            }
        );
        // A turn's init names the terminal-only commands and a command the list lacks.
        view.set_init(&json!({"slash_commands": ["compact", "code-review", "anthropic-skills:docs", "mcp__srv__prompt"],
            "terminal_slash_commands": ["doctor", "color", "focus", "reload-plugins"]}));
        let menu = view.visible();
        assert!(!names(&menu).contains(&"color"));
        assert!(!names(&menu).contains(&"heapdump"));
        let extra = menu.iter().find(|c| c.name == "mcp__srv__prompt").unwrap();
        assert_eq!(extra.description, None);
        // Each name once.
        let mut sorted = names(&menu);
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), menu.len());
    }

    #[test]
    fn switching_names_include_the_aliases_the_cli_lists() {
        let cache: CommandCache = Arc::default();
        let names = cache.lock().switching_names();
        for name in ["clear", "resume", "reset", "new", "continue"] {
            assert!(names.iter().any(|n| n == name), "{name} in {names:?}");
        }
        // A later CLI that gives `clear` another alias.
        let init = json!({"commands": [{"name": "clear", "aliases": ["reset", "new", "wipe"]}]});
        menu_from_initialize(&cache, Path::new("C:\\w"), &init);
        assert!(cache.lock().switching_names().iter().any(|n| n == "wipe"));
        // Aliases of other commands are not switching names.
        assert!(!cache.lock().switching_names().iter().any(|n| n == "review"));
    }

    #[test]
    fn the_terminal_list_of_the_last_init_applies_before_the_first_turn() {
        let cache: CommandCache = Arc::default();
        let a = Path::new("C:\\a");
        let b = Path::new("C:\\b");
        let init = json!({"commands": listed()});
        assert!(names(&menu_from_initialize(&cache, a, &init)).contains(&"color"));
        cache.lock().remember_terminal(a, &["color".to_owned()]);
        // The same directory, and another one where no turn ran yet.
        assert!(!names(&menu_from_initialize(&cache, a, &init)).contains(&"color"));
        assert!(!names(&menu_from_initialize(&cache, b, &init)).contains(&"color"));
        assert_eq!(cache.lock().menu(b).map(|m| m.len()), Some(10));
    }

    #[test]
    fn hidden_commands_have_reasons() {
        for (name, reason) in HIDDEN_COMMANDS {
            assert!(!name.is_empty() && !reason.is_empty());
        }
    }
}
