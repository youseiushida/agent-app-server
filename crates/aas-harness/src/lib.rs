//! The seam between the engine (`aas-core`) and the harness adapters.
//!
//! `aas-core` depends only on the traits here; each adapter crate implements them for one
//! agent CLI. The daemon builds the adapters from configuration and injects them into the
//! engine (constructor injection, no container).
//!
//! # Contract for adapters
//!
//! * State comes only from explicit protocol signals of the CLI. Never infer state from
//!   human-readable output or from silence (see `CLAUDE.md`, "ヒューリスティック方針").
//! * Every session emits exactly one [`AdapterEvent::Exited`], as its last event, when its
//!   process tree is gone. The events channel closes right after it.
//! * [`SessionControl::send`] is only called while no turn runs; [`SessionControl::steer`]
//!   (and [`SessionControl::steer_message`]) only while one runs and only when
//!   [`HarnessCapabilities::steer`] is set. The engine serializes the calls of one session,
//!   except the read-only [`SessionControl::status`] and [`SessionControl::side_question`].
//! * A turn ends with exactly one [`AdapterEvent::TurnCompleted`]. Items still open at that
//!   point are closed by the engine.
//! * [`AdapterEvent::TurnStarted`] may also arrive without a preceding `send` when the CLI
//!   starts a run by itself (hooks, extensions, background notifications); the engine then
//!   records an agent-initiated turn.
//! * When the process dies mid-turn an adapter may emit `TurnCompleted { status: Failed }`
//!   before `Exited`, or only `Exited`; the engine fails a turn that is still open.
//! * [`AdapterEvent::SessionInfo`] reports what the CLI says is current. The model is recorded
//!   on turns only (a CLI resolves `opus` to a full id; the user's choice stays). The permission
//!   mode, and the effort, are reflected into the thread's settings (see "Settings the harness
//!   changes by itself"); an adapter reports the effort only when the CLI states it explicitly.
//! * [`AdapterEvent::SessionIdentified`] with an id other than the session's known one means
//!   the CLI moved to another native session by itself (a command, an extension): the engine
//!   follows it and tells the user. Adapters report it whenever the CLI's own state shows such a
//!   switch (e.g. pi's `get_state.sessionId` compared at every turn end).
//! * Every child process is spawned through [`AdapterContext::supervisor`].
//! * Anything the adapter cannot map is forwarded as [`AdapterEvent::Native`] instead of
//!   being dropped or guessed.
//! * [`AdapterEvent::TurnCompleted::trigger`] is set only when the harness says explicitly why
//!   it started a run by itself (e.g. Claude's `result.origin`).
//! * [`SessionControl::send`] fails with [`AdapterError::TurnInProgress`] (and nothing else)
//!   when the adapter has seen the CLI start a run by itself that is still going on; the
//!   adapter emits that run's [`AdapterEvent::TurnStarted`] before it returns the error. The
//!   engine then sends the input again once that run has completed.
//!
//! # Background work
//!
//! Work the harness runs outside the turn lifecycle (background agents, shells, workflows,
//! monitors, scheduled wakeups, …) is reported as background tasks
//! ([`AdapterEvent::BackgroundTask`], capability `backgroundTasks`):
//!
//! * Every event carries the whole state of one task ([`BackgroundTaskInfo`]); sending the same
//!   state again changes nothing. Tasks are identified by the harness's own id (`key`), unique
//!   within the session.
//! * Whether the session is busy comes only from the harness's live set (a level signal, e.g.
//!   Claude's `background_tasks_changed`, Codex's background terminal list): the tasks with
//!   `live && !ambient` are exactly the harness's live set of work that counts as activity. A
//!   harness that replaces its set with every report is mirrored by setting `live` on every
//!   task in it and clearing it on every other one ([`BackgroundTasks::replace_live`]). The
//!   engine keeps the process alive (no idle stop), holds a sleep lease and makes a drain wait
//!   while such a task exists. `ambient` tasks ("not activity") never do.
//! * `state` comes from the harness's start / end signals. A task ends only by an explicit
//!   terminal signal of the harness, or with the process ([`AdapterEvent::Exited`] ends every
//!   task that has not ended; the engine records how). Adapters never end a task because of
//!   elapsed time, and never infer a result from text written for people.
//! * A task that starts again under the same key after it ended is a new run: `runs` goes up
//!   by one and `state` is `Running` again.
//! * An item whose work goes on as a task: the adapter emits the task (with
//!   `origin_item_key`) before it closes the item with [`ItemStatus::Backgrounded`], and both
//!   before the turn's [`AdapterEvent::TurnCompleted`].
//! * [`AdapterEvent::InteractionRequested::background_key`] names the task that asks. Such a
//!   request belongs to the task, not to the turn: it outlives the turn and expires when the
//!   task ends.
//! * [`SessionControl::stop_background`] only asks: `Ok` means the harness accepted the request.
//!   The end arrives as a task in a terminal state (or with `Exited`).
//! * What a running task prints is reported with [`AdapterEvent::BackgroundOutput`], only where
//!   the harness streams it in explicit fields of its own (a background terminal's output
//!   deltas, a shell's output snapshots), never read from text written for people. The whole
//!   output the harness reports at the end goes into [`BackgroundOutcome::output`]; an adapter
//!   that reads it from a file the harness names reads at most
//!   [`AdapterPolicy::max_output_file_bytes`] (the end of it) and says how much it left out
//!   ([`BackgroundOutcome::output_omitted_bytes`]).
//!
//! # Requests the engine no longer needs answered
//!
//! When a request expires in the engine while the process lives (its turn or its background
//! task ended), the engine calls [`SessionControl::expire_request`] so that the adapter answers
//! the CLI, which would otherwise wait for an answer forever. A request the CLI withdrew
//! ([`AdapterEvent::InteractionWithdrawn`]) needs no answer.
//!
//! # Features beyond the capabilities
//!
//! [`HarnessAdapter::features`] ([`HarnessFeatures`], published as `Harness.features`) says what
//! the harness offers besides [`HarnessCapabilities`]. The engine calls the matching methods
//! only when the feature is on; each has a default that refuses (or reports nothing), so an
//! adapter implements exactly what its CLI supports:
//!
//! | feature | methods and events |
//! |---|---|
//! | `forkAtTurn` | [`AdapterEvent::TurnAnchor`], [`AdapterEvent::TurnAnchorReplaced`]; [`StartOptions::fork_at`] with [`StartMode::Fork`] |
//! | `forkWhileHeld` | (a fork of a session another process holds starts like any fork) |
//! | `rename` | [`SessionControl::rename`]; native renames arrive as [`AdapterEvent::SessionTitle`] |
//! | `sideQuestion` | [`SessionControl::side_question`] |
//! | `moveToBackground` | [`AdapterEvent::ItemBackgroundable`]; [`SessionControl::move_to_background`] |
//! | `status` | [`SessionControl::status`], [`HarnessAdapter::status`] |
//! | `projectTrust` | [`StartOptions::project_trusted`] |
//! | `planMode` | [`StartOptions::modes`], [`SessionControl::apply_modes`], [`AdapterEvent::ModesReported`], [`ItemBody::ProposedPlan`] items |
//! | `fastModeModels` | the same, for `fast` |
//!
//! [`HarnessAdapter::start_with`] is what the engine calls to start a session; its default
//! calls [`HarnessAdapter::start`] and ignores the options (none are set for an adapter whose
//! features do not name them).
//!
//! # Turn anchors (fork at a turn)
//!
//! While a turn runs (or right when it ends, before its `TurnCompleted`), an adapter with the
//! feature `forkAtTurn` reports the CLI's own anchor of that turn with
//! [`AdapterEvent::TurnAnchor`]: whatever the CLI needs later to branch the session at that
//! turn, taken from explicit fields the CLI sent for this very turn (a turn id, the uuid of its
//! last message, an entry id, a step id), never by counting turns or messages. The value is the
//! adapter's own (JSON); the engine stores it with the turn, copies it into forks, and hands it
//! back unchanged in a [`ForkPoint`]. The latest report of a turn wins; a CLI that settles the
//! anchor of a turn only later (in the next turn) replaces it with
//! [`AdapterEvent::TurnAnchorReplaced`], naming the turn by its earlier anchor. A turn without an
//! anchor cannot be forked at (`Turn.forkable`). [`HarnessAdapter::read_native_history_anchored`] gives
//! the anchors of imported turns when the CLI's history carries them.
//!
//! An anchor belongs to the native session it was reported in. When the harness moves the
//! thread to another native session ([`AdapterEvent::SessionIdentified`] with another id), the
//! engine keeps the earlier anchors with the session they came from: a fork at such a turn
//! branches that session ([`StartMode::Fork`] names it), and a [`AdapterEvent::TurnAnchorReplaced`]
//! only replaces anchors of the session the thread runs now. [`ForkPoint::previous`] is always
//! an anchor of the same session as [`ForkPoint::anchor`].
//!
//! # Settings the harness changes by itself
//!
//! Harnesses change their permission mode by themselves (Claude Code's "allow for this session"
//! suggestion that switches to `acceptEdits`, leaving plan mode after its approval, Devin's
//! `/plan` and `/ask` commands). The adapter reports what the CLI says
//! ([`AdapterEvent::SessionInfo`], [`AdapterEvent::ModesReported`]) and the engine reflects it
//! into the thread (`settings.permissionMode`, `settings.effort`, `modes.plan`) — unless the user
//! changed the same value and it waits for the next turn, which the engine then applies. Values
//! outside the harness's advertised lists are not taken over (logged). The process already runs
//! with a reflected value, so the engine does not apply it again.
//!
//! # Commands that switch sessions
//!
//! [`HarnessAdapter::session_switching_names`] names the harness's commands that switch the
//! native session inside the running process, with their aliases as the harness lists them.
//! The engine never offers them in `command/list` and refuses input whose first word is one of
//! them (`sessionSwitchingCommand`): a thread is one native session. Adapters may list a
//! command's aliases as commands of their own (`/review` for Claude's `code-review`).
//!
//! # Errors
//!
//! [`AdapterError`]'s text never repeats its own prefix and never carries terminal escape
//! sequences ([`sanitize_terminal_text`]). Adapters attach a process's stderr with
//! [`AdapterPolicy::with_stderr`] (the one place stderr becomes part of a message);
//! [`AdapterError::detail`] is the text without the daemon's prefix, which clients show after a
//! lead-in in their own language.

use std::collections::BTreeMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::mpsc;

pub use aas_protocol as protocol;
pub use aas_protocol::{
    BackgroundProgress, BackgroundTaskKind, BackgroundUsage, Command, ContextUsage, DeltaField,
    EffortLevel, ExpireReason, HarnessCapabilities, HarnessFeatures, HarnessKind,
    InteractionRequest, InteractionResolution, ItemBody, ItemStatus, Millis, Model, NoticeLevel,
    PermissionMode, PlanModeFeature, StatusRow, StatusSection, ThreadId, ThreadModes,
    ThreadSettings, TurnError, TurnStatus, TurnTrigger, Usage, WorkflowAgent, WorkflowAgentState,
};
pub use aas_supervisor::{ExitInfo, StopReason, Supervisor};

/// Configuration of one harness, as written in `config.toml` (`[[harness]]`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HarnessConfig {
    /// Stable id used by the protocol (`codex`, `claude`, `devin`, …).
    pub id: String,
    pub kind: HarnessKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Executable: a bare name resolved through PATH/PATHEXT, or a path.
    pub command: String,
    /// Extra arguments placed before the adapter's own arguments.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Extra environment variables for the child.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// Adapter-specific options (documented in `docs/adapters/<kind>.md`).
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub options: Value,
}

/// Policy values relevant to adapters (subset of the daemon policy, `docs/design.md` §13;
/// `Default` holds the same defaults as that table).
#[derive(Debug, Clone)]
pub struct AdapterPolicy {
    /// Wait after closing stdin before terminating the tree. Default 5 s: time for a CLI to
    /// save its session and exit on its own.
    pub stop_grace: Duration,
    /// Upper bound of a single line read from the CLI. Default 64 MiB: lines carrying images or
    /// large tool output fit, and broken output cannot exhaust memory.
    pub max_line_bytes: usize,
    /// Deadline for requests to the CLI: handshakes (initialize, session creation, listings)
    /// and the requests behind [`SessionControl`] calls (starting a turn, steering, answering,
    /// applying settings). The engine bounds those calls with it too. Default 60 s: the first
    /// start of a Node.js CLI can be slow (cold cache, antivirus scan).
    pub handshake_timeout: Duration,
    /// Characters of a native session title made from the first line of its first prompt
    /// (`policy.first_message_title_chars`, the rule the engine applies to thread titles).
    /// Default 80: fits one line of the phone's thread list.
    pub first_message_title_chars: usize,
    /// Characters kept of a title the harness gave a native session (`policy.harness_title_chars`,
    /// the rule the engine applies to `SessionTitle`). Default 200: harnesses write whole
    /// sentences, and more than two lines of the list is noise.
    pub harness_title_chars: usize,
    /// Last lines of a process's stderr quoted in an error (`policy.exit_message_stderr_lines`,
    /// the rule the engine applies to an agent that exited). Default 5: the last exception and
    /// its cause fit, and a phone can show them.
    pub stderr_excerpt_lines: usize,
    /// Upper bound of what an adapter reads of a file in which the harness kept a background
    /// task's output (`policy.max_output_file_bytes`; Claude Code's
    /// `task_notification.output_file`). A longer file is read from its end (the latest output,
    /// with the exit line the CLI appends), and the result says how much was left out.
    /// Default 8 MiB: the whole output of builds and test runs, and hours of a development
    /// server's log, while a file that grew without bound (a server logging for days) is
    /// neither held in memory whole nor stored as a blob a phone would download.
    pub max_output_file_bytes: u64,
}

impl Default for AdapterPolicy {
    fn default() -> Self {
        Self {
            stop_grace: Duration::from_secs(5),
            max_line_bytes: 64 * 1024 * 1024,
            handshake_timeout: Duration::from_secs(60),
            first_message_title_chars: 80,
            harness_title_chars: 200,
            stderr_excerpt_lines: 5,
            max_output_file_bytes: 8 * 1024 * 1024,
        }
    }
}

impl AdapterPolicy {
    /// `error` with the last [`stderr_excerpt_lines`](Self::stderr_excerpt_lines) lines of a
    /// process's stderr (`stderr_tail`, e.g. [`ExitInfo::stderr_tail`]) added to its text,
    /// escape sequences removed (see [`AdapterError::with_stderr`]).
    pub fn with_stderr(&self, error: AdapterError, stderr_tail: &str) -> AdapterError {
        error.with_stderr(stderr_tail, self.stderr_excerpt_lines)
    }

    /// Title of a native session made from its first prompt (see [`title_from_first_line`]).
    pub fn prompt_title(&self, prompt: &str) -> Option<String> {
        let title = title_from_first_line(prompt, self.first_message_title_chars);
        (!title.is_empty()).then_some(title)
    }

    /// Title a harness gave a native session (see [`harness_title`]).
    pub fn harness_title(&self, title: &str) -> Option<String> {
        let title = harness_title(title, self.harness_title_chars);
        (!title.is_empty()).then_some(title)
    }
}

/// A title from the first non-empty line of `text`, trimmed, at most `max_chars` characters; a
/// longer line is cut and ends with `…`. The one rule for titles made from a message: the engine
/// uses it for thread titles and adapters for native sessions without a name.
pub fn title_from_first_line(text: &str, max_chars: usize) -> String {
    let first = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    let mut title: String = first.chars().take(max_chars).collect();
    if first.chars().count() > max_chars {
        title.push('…');
    }
    title
}

/// A title a harness gave a session, trimmed and cut to `max_chars` characters. The one rule
/// for harness-given titles: the engine uses it for `SessionTitle`, adapters for native
/// session names.
pub fn harness_title(title: &str, max_chars: usize) -> String {
    title.trim().chars().take(max_chars).collect()
}

/// `text` without terminal control: ANSI / VT escape sequences (ECMA-48 CSI, OSC, DCS, SOS, PM
/// and APC strings, and the other escape sequences), C1 control characters, and the C0 control
/// characters other than line feed and tab. Line breaks become `\n` (`\r\n` and a lone `\r`).
///
/// A CLI writes colours and cursor movement to its stderr for a terminal; the daemon shows
/// that text on a phone, where the codes would appear as garbage. The sequences are recognised
/// by their syntax (ECMA-48), not by what they mean, so nothing is guessed: the text between
/// them is kept as it is.
pub fn sanitize_terminal_text(text: &str) -> String {
    const ESC: char = '\u{1b}';
    const BEL: char = '\u{07}';
    const C1_CSI: char = '\u{9b}';
    const C1_ST: char = '\u{9c}';
    const C1_OSC: char = '\u{9d}';
    /// Introducers of strings ended by ST: DCS, SOS, PM, APC (C1 and 7-bit forms).
    fn is_string_c1(c: char) -> bool {
        matches!(c, '\u{90}' | '\u{98}' | '\u{9e}' | '\u{9f}')
    }
    fn is_string_7bit(c: char) -> bool {
        matches!(c, 'P' | 'X' | '^' | '_')
    }
    /// Skips a control sequence's parameter and intermediate bytes and its final byte.
    fn skip_csi(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
        for c in chars.by_ref() {
            if ('\u{40}'..='\u{7e}').contains(&c) {
                return;
            }
            if !('\u{20}'..='\u{3f}').contains(&c) {
                // Not a well-formed sequence: it ends here.
                return;
            }
        }
    }
    /// Skips a command string up to its terminator (ST as `ESC \` or C1, or BEL for OSC).
    fn skip_string(chars: &mut std::iter::Peekable<std::str::Chars<'_>>, bel_ends: bool) {
        while let Some(c) = chars.next() {
            match c {
                BEL if bel_ends => return,
                C1_ST => return,
                ESC => {
                    if chars.peek() == Some(&'\\') {
                        chars.next();
                    }
                    return;
                }
                _ => {}
            }
        }
    }
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            ESC => match chars.next() {
                Some('[') => skip_csi(&mut chars),
                Some(']') => skip_string(&mut chars, true),
                Some(c) if is_string_7bit(c) => skip_string(&mut chars, false),
                // nF sequences: intermediate bytes, then a final byte.
                Some(c) if ('\u{20}'..='\u{2f}').contains(&c) => {
                    for c in chars.by_ref() {
                        if !('\u{20}'..='\u{2f}').contains(&c) {
                            break;
                        }
                    }
                }
                // Fp, Fe, Fs sequences: one final byte (already consumed); a lone ESC at the
                // end is dropped.
                _ => {}
            },
            C1_CSI => skip_csi(&mut chars),
            C1_OSC => skip_string(&mut chars, true),
            c if is_string_c1(c) => skip_string(&mut chars, false),
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push('\n');
            }
            '\n' | '\t' => out.push(c),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

/// The last `max_lines` lines of a process's stderr (`tail`), without terminal control
/// ([`sanitize_terminal_text`]) and surrounding blank space: what an error quotes of it. Empty
/// when nothing is left.
pub fn stderr_excerpt(tail: &str, max_lines: usize) -> String {
    let clean = sanitize_terminal_text(tail);
    let lines: Vec<&str> = clean.trim().lines().collect();
    let skip = lines.len().saturating_sub(max_lines);
    lines[skip..].join("\n").trim().to_owned()
}

/// Dependencies injected into adapters.
#[derive(Clone)]
pub struct AdapterContext {
    pub supervisor: Supervisor,
    /// Directory private to this adapter instance (e.g. files the adapter installs for the CLI).
    pub state_dir: PathBuf,
    pub policy: AdapterPolicy,
}

/// Result of [`HarnessAdapter::probe`].
#[derive(Debug, Clone, PartialEq)]
pub struct HarnessInfo {
    pub available: bool,
    pub unavailable_reason: Option<String>,
    pub version: Option<String>,
    pub executable: Option<PathBuf>,
    pub capabilities: HarnessCapabilities,
    pub models: Vec<Model>,
    pub default_model: Option<String>,
    pub effort_levels: Vec<EffortLevel>,
    pub permission_modes: Vec<PermissionMode>,
    pub default_permission_mode: Option<String>,
}

impl HarnessInfo {
    /// Info for a harness that cannot be used.
    pub fn unavailable(reason: impl Into<String>) -> Self {
        Self {
            available: false,
            unavailable_reason: Some(reason.into()),
            version: None,
            executable: None,
            capabilities: HarnessCapabilities::default(),
            models: Vec::new(),
            default_model: None,
            effort_levels: Vec::new(),
            permission_modes: Vec::new(),
            default_permission_mode: None,
        }
    }
}

/// How a session starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartMode {
    /// A brand new native session.
    New,
    /// Continue an existing native session.
    Resume { native_session_id: String },
    /// Branch a new native session off an existing one (capability `fork`).
    Fork { native_session_id: String },
}

/// Arguments of [`HarnessAdapter::start`].
#[derive(Debug, Clone)]
pub struct StartRequest {
    pub thread_id: ThreadId,
    pub cwd: PathBuf,
    pub settings: ThreadSettings,
    pub mode: StartMode,
}

/// What [`HarnessAdapter::start_with`] adds to a [`StartRequest`]. The engine sets only what the
/// adapter's [`HarnessFeatures`] offer; everything else stays at its default. An adapter that
/// offers a feature honours its option here, or fails the start with an error that says why
/// (it never starts a session that silently lacks it: a fork at a turn that copies the whole
/// session would hold turns the new thread does not show).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StartOptions {
    /// The modes the session starts in (plan mode with the feature `planMode`, fast mode for a
    /// model of `fastModeModels`). A session starts with both off unless they are set here.
    pub modes: ThreadModes,
    /// With [`StartMode::Fork`]: branch the source at this point instead of copying the whole
    /// session (feature `forkAtTurn`).
    pub fork_at: Option<ForkPoint>,
    /// The user's decision whether the harness may load the project's own resources (feature
    /// `projectTrust`); `None`: not decided, the CLI's own saved decision applies.
    pub project_trusted: Option<bool>,
}

/// Where a fork at a turn branches the source session ([`StartOptions::fork_at`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ForkPoint {
    /// The anchor the adapter reported for the turn, unchanged: the latest of
    /// [`AdapterEvent::TurnAnchor`] and [`AdapterEvent::TurnAnchorReplaced`].
    pub anchor: Value,
    /// `false`: the branch holds the turn and everything before it. `true`: it holds everything
    /// before the turn (the user edits the turn's prompt). The engine never asks for "before
    /// the first turn": that is a new session.
    pub before: bool,
    /// Where the source holds everything before the turn, for CLIs that cut after the last
    /// message they keep: the anchor of the nearest earlier turn that reached the agent (turns
    /// whose start failed are not in the session and are skipped), when that turn's anchor was
    /// recorded in the same native session as `anchor`. `None` when there is no such anchor;
    /// an adapter that needs one refuses the point ([`HarnessAdapter::check_fork_point`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous: Option<Value>,
}

/// What [`HarnessAdapter::upgrade_settings`] makes of settings of an earlier form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpgradedSettings {
    pub settings: ThreadSettings,
    /// The settings asked for plan mode ([`ThreadModes::plan`]).
    pub plan: bool,
}

/// The answer to [`SessionControl::side_question`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SideAnswer {
    /// The answer, verbatim; `None` when the harness gave none.
    pub answer: Option<String>,
    /// The harness says the answer did not come from the model.
    pub synthetic: bool,
}

/// One piece of user input, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnInputPart {
    Text(String),
    /// An image stored on disk by the engine.
    Image {
        path: PathBuf,
        mime: String,
    },
    /// A file or directory mentioned with `@`: `relative` as the user typed it (forward
    /// slashes, relative to the thread's cwd), `absolute` resolved by the engine.
    Mention {
        relative: String,
        absolute: PathBuf,
    },
}

/// User input of a turn (or a steer).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TurnInput {
    pub parts: Vec<TurnInputPart>,
}

impl TurnInput {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            parts: vec![TurnInputPart::Text(text.into())],
        }
    }

    /// Plain-text rendering (text parts verbatim, mentions as `@path`, images omitted).
    pub fn to_plain_text(&self) -> String {
        let mut out = String::new();
        for part in &self.parts {
            match part {
                TurnInputPart::Text(t) => out.push_str(t),
                TurnInputPart::Mention { relative, .. } => {
                    if !out.is_empty() && !out.ends_with(char::is_whitespace) {
                        out.push(' ');
                    }
                    out.push('@');
                    out.push_str(relative);
                }
                TurnInputPart::Image { .. } => {}
            }
        }
        out
    }

    pub fn images(&self) -> impl Iterator<Item = (&Path, &str)> {
        self.parts.iter().filter_map(|p| match p {
            TurnInputPart::Image { path, mime } => Some((path.as_path(), mime.as_str())),
            _ => None,
        })
    }
}

/// Outcome of [`SessionControl::apply_settings`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsApplied {
    /// The running process now uses the new settings.
    Live,
    /// The process must be restarted (resumed) for the settings to take effect.
    RequiresRestart,
}

/// Lifecycle state of a background task, from the harness's start and end signals.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum BackgroundState {
    Running,
    Completed,
    Failed,
    /// Stopped (on request, or by the harness itself).
    Stopped,
}

impl BackgroundState {
    pub fn is_ended(self) -> bool {
        !matches!(self, BackgroundState::Running)
    }
}

/// What a background task produced, only from explicit fields of the harness.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackgroundOutcome {
    /// The harness's summary of what the task did, verbatim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// Exit code of a command, when the harness reports it in a field of its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// The whole output (the engine keeps `policy.max_inline_output_bytes` inline and moves
    /// the rest to a blob).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    /// Bytes at the start of the output that were not read: the harness kept the output in a
    /// file larger than [`AdapterPolicy::max_output_file_bytes`], and `output` is its end.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_omitted_bytes: Option<u64>,
}

/// More output of a running background task ([`AdapterEvent::BackgroundOutput`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "text", rename_all = "camelCase")]
pub enum OutputUpdate {
    /// What the task printed after everything reported before (a harness that streams
    /// deltas).
    Append(String),
    /// Everything the current run printed so far, replacing what was reported before (a
    /// harness that reports snapshots of the whole output, when a snapshot does not continue
    /// the previous one).
    Replace(String),
}

/// Work the harness runs outside the turn lifecycle, as the adapter knows it (whole state; see
/// the crate docs, "Background work").
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackgroundTaskInfo {
    /// The harness's id of the task, unique within the session (Claude `task_id`, Codex
    /// process id or child thread id).
    pub key: String,
    pub kind: BackgroundTaskKind,
    /// The harness's description, verbatim.
    pub title: String,
    /// Member of the harness's live set (a level signal).
    pub live: bool,
    /// The harness says the task is not activity: it never keeps the session busy.
    #[serde(default)]
    pub ambient: bool,
    pub state: BackgroundState,
    /// Starts under this key (1 for the first run).
    pub runs: u32,
    /// Key of the item that launched the task (in the turn that ran then).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_item_key: Option<String>,
    /// Key of the background task that launched this one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<BackgroundProgress>,
    /// Present once the harness reported what the task produced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<BackgroundOutcome>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<BackgroundUsage>,
    /// [`SessionControl::stop_background`] can stop this task.
    #[serde(default)]
    pub stoppable: bool,
    /// When the harness says the task runs next (scheduled wakeups), as the harness reported it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_run_at: Option<Millis>,
}

impl BackgroundTaskInfo {
    /// A task that just started: running, live, first run, nothing reported yet.
    pub fn new(key: impl Into<String>, kind: BackgroundTaskKind, title: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            kind,
            title: title.into(),
            live: true,
            ambient: false,
            state: BackgroundState::Running,
            runs: 1,
            origin_item_key: None,
            parent_key: None,
            progress: None,
            result: None,
            usage: None,
            stoppable: false,
            next_run_at: None,
        }
    }

    /// Whether the task keeps the session busy: in the harness's live set and not ambient.
    pub fn keeps_busy(&self) -> bool {
        self.live && !self.ambient
    }
}

/// An entry of a harness's live set (see [`BackgroundTasks::replace_live`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveEntry {
    pub key: String,
    pub ambient: bool,
}

/// The background tasks of one session as an adapter tracks them. It applies the harness's
/// signals — starts, ends and live-set reports — with the rules of the contract (crate docs,
/// "Background work") and returns the whole state of every task that changed, for the adapter
/// to emit as [`AdapterEvent::BackgroundTask`]. It never ends a task by itself.
#[derive(Debug, Clone, Default)]
pub struct BackgroundTasks {
    tasks: BTreeMap<String, BackgroundTaskInfo>,
}

impl BackgroundTasks {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, key: &str) -> Option<&BackgroundTaskInfo> {
        self.tasks.get(key)
    }

    /// Every task the session reported, ended ones included (by key).
    pub fn iter(&self) -> impl Iterator<Item = &BackgroundTaskInfo> {
        self.tasks.values()
    }

    /// Whether any task keeps the session busy.
    pub fn any_busy(&self) -> bool {
        self.tasks.values().any(BackgroundTaskInfo::keeps_busy)
    }

    /// The harness reported that a task started. A key that is not known yet is added as it
    /// is. A known key that had ended starts a new run (`runs` + 1, running, no result). A
    /// known running key takes the new details. The live set is the harness's level signal:
    /// for a known key `live` and `ambient` stay as the last report said. Returns the task's
    /// new state when it changed.
    pub fn started(&mut self, task: BackgroundTaskInfo) -> Option<BackgroundTaskInfo> {
        let Some(known) = self.tasks.get_mut(&task.key) else {
            let task = BackgroundTaskInfo {
                runs: task.runs.max(1),
                ..task
            };
            self.tasks.insert(task.key.clone(), task.clone());
            return Some(task);
        };
        let before = known.clone();
        // A new run starts from scratch; a repeated start of the running task only adds what
        // it carries.
        let restart = before.state.is_ended();
        fn keep<T>(restart: bool, new: Option<T>, old: Option<T>) -> Option<T> {
            if restart { new } else { new.or(old) }
        }
        *known = BackgroundTaskInfo {
            key: task.key,
            kind: task.kind,
            title: task.title,
            live: before.live,
            ambient: before.ambient,
            state: BackgroundState::Running,
            runs: if restart {
                before.runs + 1
            } else {
                before.runs
            },
            origin_item_key: task.origin_item_key.or(before.origin_item_key.clone()),
            parent_key: task.parent_key.or(before.parent_key.clone()),
            progress: keep(restart, task.progress, before.progress.clone()),
            result: keep(restart, task.result, before.result.clone()),
            usage: keep(restart, task.usage, before.usage),
            stoppable: task.stoppable,
            next_run_at: keep(restart, task.next_run_at, before.next_run_at),
        };
        (*known != before).then(|| known.clone())
    }

    /// Applies `change` to a known task. Returns its new state when it changed (`None` for an
    /// unknown key, which is left alone).
    pub fn update(
        &mut self,
        key: &str,
        change: impl FnOnce(&mut BackgroundTaskInfo),
    ) -> Option<BackgroundTaskInfo> {
        let task = self.tasks.get_mut(key)?;
        let before = task.clone();
        change(task);
        (*task != before).then(|| task.clone())
    }

    /// The harness reported the end of a task (`state` is not `Running`), with what it
    /// produced. Returns the new state when it changed.
    pub fn ended(
        &mut self,
        key: &str,
        state: BackgroundState,
        result: Option<BackgroundOutcome>,
    ) -> Option<BackgroundTaskInfo> {
        debug_assert!(state.is_ended(), "an end must be a terminal state");
        self.update(key, |task| {
            task.state = state;
            if result.is_some() {
                task.result = result;
            }
        })
    }

    /// The harness reported its live set: exactly the tasks of `live` are live now (with
    /// their `ambient` flag), every other task is not. A key that is not known yet is added
    /// as `unknown` makes it (the live set may arrive before the start signal). Returns every
    /// task whose state changed.
    pub fn replace_live(
        &mut self,
        live: impl IntoIterator<Item = LiveEntry>,
        mut unknown: impl FnMut(&LiveEntry) -> BackgroundTaskInfo,
    ) -> Vec<BackgroundTaskInfo> {
        let live: BTreeMap<String, LiveEntry> =
            live.into_iter().map(|e| (e.key.clone(), e)).collect();
        let mut changed = Vec::new();
        for (key, entry) in &live {
            if !self.tasks.contains_key(key) {
                let task = BackgroundTaskInfo {
                    key: key.clone(),
                    live: true,
                    ambient: entry.ambient,
                    ..unknown(entry)
                };
                self.tasks.insert(key.clone(), task.clone());
                changed.push(task);
            }
        }
        for (key, task) in self.tasks.iter_mut() {
            let (now_live, now_ambient) = match live.get(key) {
                Some(entry) => (true, entry.ambient),
                None => (false, task.ambient),
            };
            if task.live != now_live || task.ambient != now_ambient {
                task.live = now_live;
                task.ambient = now_ambient;
                if !changed.iter().any(|c: &BackgroundTaskInfo| &c.key == key) {
                    changed.push(task.clone());
                }
            }
        }
        changed
    }
}

/// Normalized events emitted by a session. Items are identified by an adapter-chosen `key`
/// (unique within the session); the engine maps keys to protocol item ids.
#[derive(Debug, Clone, PartialEq)]
pub enum AdapterEvent {
    /// The native session id became known or changed (e.g. a fork got its own id). A change of
    /// an id the thread already had is a switch the CLI made by itself: the engine follows it
    /// and tells the user (`thread/nativeSessionChanged`).
    SessionIdentified {
        native_session_id: String,
    },
    /// Model / permission mode / effort the CLI reports as current. The permission mode and the
    /// effort are reflected into the thread's settings; the model is recorded on turns only.
    SessionInfo {
        model: Option<String>,
        permission_mode: Option<String>,
        effort: Option<String>,
    },
    /// The harness-native command list changed.
    CommandsChanged {
        commands: Vec<Command>,
    },
    /// A title the harness gave the session (e.g. an auto-generated thread name).
    SessionTitle {
        title: String,
    },
    /// Harness-level information (models, modes, effort levels) may have changed; the engine
    /// re-probes the harness and publishes the result.
    HarnessInfoChanged,
    /// The CLI acknowledged the start of a turn.
    TurnStarted,
    ItemStarted {
        key: String,
        body: ItemBody,
    },
    ItemDelta {
        key: String,
        field: DeltaField,
        text: String,
    },
    /// Replaces the item's body (non-append changes: plan entries, file lists, …).
    ItemUpdated {
        key: String,
        body: ItemBody,
    },
    /// Closes an item. `body = None` keeps what was accumulated from deltas.
    ItemCompleted {
        key: String,
        body: Option<ItemBody>,
        status: ItemStatus,
    },
    /// The CLI asks the user something; answer with [`SessionControl::respond`].
    InteractionRequested {
        request_id: String,
        request: InteractionRequest,
        item_key: Option<String>,
        /// The background task that asks (its key), when the harness says so explicitly
        /// (e.g. Claude's `can_use_tool.agent_id`). The request then belongs to that task: it
        /// outlives the turn and expires when the task ends. `None`: it belongs to the running
        /// turn, or to the thread when no turn runs.
        background_key: Option<String>,
    },
    /// The CLI withdrew a pending request (it no longer needs an answer).
    InteractionWithdrawn {
        request_id: String,
    },
    /// Usage of the current turn so far (cumulative within the turn). The engine relays it
    /// as `turn/usageUpdated`. `usage.context` is set only when the CLI explicitly reports
    /// both the context-window size and its occupancy; adapters never estimate it (token
    /// counting, model tables) and leave it `None` otherwise.
    TurnUsage {
        usage: Usage,
    },
    /// The turn ended. A `usage` without `context` keeps the context the turn last reported.
    TurnCompleted {
        status: TurnStatus,
        usage: Option<Usage>,
        error: Option<TurnError>,
        /// Why the harness started this run by itself, when it says so explicitly.
        trigger: Option<TurnTrigger>,
    },
    /// The whole current state of one background task (see the crate docs, "Background
    /// work"). Idempotent. Boxed: the state is large next to the other events.
    BackgroundTask {
        task: Box<BackgroundTaskInfo>,
    },
    /// Output of the current run of the running background task `key`, as the harness streams
    /// it explicitly (see the crate docs, "Background work"). The engine keeps the first
    /// `policy.max_inline_output_bytes` of a run and relays them (`backgroundTask/outputDelta`);
    /// the whole output comes with the task's end ([`BackgroundOutcome::output`]). Output of a
    /// task the engine does not know, or that has ended, is ignored.
    BackgroundOutput {
        key: String,
        output: OutputUpdate,
    },
    /// Shown to the user as a notice item.
    Notice {
        level: NoticeLevel,
        message: String,
        code: Option<String>,
    },
    /// The CLI's own anchor of the running turn (see the crate docs, "Turn anchors"). The
    /// latest report before the turn's `TurnCompleted` wins.
    TurnAnchor {
        anchor: Value,
    },
    /// The anchor of a turn of this thread (the running one or an earlier one), the one whose
    /// recorded anchor is exactly `previous`, becomes `anchor`: for CLIs that settle what a turn
    /// is branched at only after the turn (Devin's step node ids, notified with the next
    /// prompt). The turn is named by its own earlier anchor, never by its position. A
    /// replacement that matches no turn changes nothing (the engine logs a warning).
    TurnAnchorReplaced {
        previous: Value,
        anchor: Value,
    },
    /// Plan mode and fast mode as the CLI reports them (see "Settings the harness changes by
    /// itself"). `fast_state` is the CLI's own word for fast mode (display only).
    ModesReported {
        plan: Option<bool>,
        fast_state: Option<String>,
    },
    /// The CLI did not take a steer sent with [`SessionControl::steer_message`] into the running
    /// turn and the adapter withdrew it: the engine puts it back at the front of the queue.
    SteerReturned {
        message_id: String,
    },
    /// The CLI asks to put `text` into the composer (e.g. a pi extension's `setEditorText`).
    ComposerText {
        text: String,
    },
    /// Whether the running item `key` can be moved to the background now
    /// ([`SessionControl::move_to_background`]), as the CLI says explicitly.
    ItemBackgroundable {
        key: String,
        backgroundable: bool,
    },
    /// A CLI message the adapter does not map.
    Native {
        payload: Value,
    },
    /// The process tree is gone. Always the last event.
    Exited {
        info: ExitInfo,
    },
}

/// A running session.
pub struct SessionHandle {
    /// Known at start for most harnesses; otherwise reported via [`AdapterEvent::SessionIdentified`].
    pub native_session_id: Option<String>,
    pub control: Arc<dyn SessionControl>,
    pub events: mpsc::UnboundedReceiver<AdapterEvent>,
}

/// Errors reported by adapters.
///
/// The text (`Display`) is a prefix naming the kind of error, then [`detail`](Self::detail):
/// the error's own text without terminal escape sequences and without prefixes of this type
/// that an adapter carried over by formatting another `AdapterError` into it (so no prefix is
/// ever repeated).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AdapterError {
    Unavailable(String),
    Unsupported(&'static str),
    Spawn(String),
    Protocol(String),
    Harness(String),
    Closed,
    UnknownRequest(String),
    /// [`SessionControl::send`] found the CLI running a turn it started by itself (the
    /// adapter has emitted that turn's `TurnStarted`); the input was not taken.
    TurnInProgress,
    Other(String),
}

/// The prefixes of [`AdapterError`]'s text, by kind.
const UNAVAILABLE_PREFIX: &str = "harness unavailable: ";
const UNSUPPORTED_PREFIX: &str = "not supported by this harness: ";
const SPAWN_PREFIX: &str = "failed to start the harness: ";
const PROTOCOL_PREFIX: &str = "unexpected message from the harness: ";
const HARNESS_PREFIX: &str = "the harness reported an error: ";
const UNKNOWN_REQUEST_PREFIX: &str = "unknown request id ";
const CLOSED_TEXT: &str = "the session is closed";
const TURN_IN_PROGRESS_TEXT: &str = "the agent is running a turn it started by itself";

impl AdapterError {
    fn prefix(&self) -> &'static str {
        match self {
            AdapterError::Unavailable(_) => UNAVAILABLE_PREFIX,
            AdapterError::Unsupported(_) => UNSUPPORTED_PREFIX,
            AdapterError::Spawn(_) => SPAWN_PREFIX,
            AdapterError::Protocol(_) => PROTOCOL_PREFIX,
            AdapterError::Harness(_) => HARNESS_PREFIX,
            AdapterError::UnknownRequest(_) => UNKNOWN_REQUEST_PREFIX,
            AdapterError::Closed | AdapterError::TurnInProgress | AdapterError::Other(_) => "",
        }
    }

    /// The error's own text: what the harness (or the adapter) said, without the prefix of
    /// its kind, without the prefixes of this type an adapter formatted into it, and without
    /// terminal control ([`sanitize_terminal_text`]). Clients show it after a lead-in of their
    /// own (`data.detail` of `adapterError`, `Turn.error.message` of a failed start).
    pub fn detail(&self) -> String {
        let raw = match self {
            AdapterError::Unavailable(s)
            | AdapterError::Spawn(s)
            | AdapterError::Protocol(s)
            | AdapterError::Harness(s)
            | AdapterError::UnknownRequest(s)
            | AdapterError::Other(s) => s.as_str(),
            AdapterError::Unsupported(capability) => capability,
            AdapterError::Closed => CLOSED_TEXT,
            AdapterError::TurnInProgress => TURN_IN_PROGRESS_TEXT,
        };
        let mut text = sanitize_terminal_text(raw);
        // The prefixes are this type's own words (never the harness's), so removing them is
        // exact: an adapter that wrote `format!("{e}; …")` of another error gets it once.
        loop {
            let trimmed = text.trim_start();
            let Some(rest) = [
                UNAVAILABLE_PREFIX,
                UNSUPPORTED_PREFIX,
                SPAWN_PREFIX,
                PROTOCOL_PREFIX,
                HARNESS_PREFIX,
            ]
            .iter()
            .find_map(|p| trimmed.strip_prefix(p)) else {
                break;
            };
            text = rest.to_owned();
        }
        text.trim().to_owned()
    }

    /// The same error with the last `max_lines` lines of a process's stderr added to its text
    /// (see [`stderr_excerpt`]); unchanged when the stderr holds nothing. Adapters use
    /// [`AdapterPolicy::with_stderr`].
    pub fn with_stderr(self, stderr_tail: &str, max_lines: usize) -> AdapterError {
        let excerpt = stderr_excerpt(stderr_tail, max_lines);
        if excerpt.is_empty() {
            return self;
        }
        let text = format!("{}\nstderr: {excerpt}", self.detail());
        match self {
            AdapterError::Unavailable(_) => AdapterError::Unavailable(text),
            AdapterError::Spawn(_) => AdapterError::Spawn(text),
            AdapterError::Protocol(_) => AdapterError::Protocol(text),
            AdapterError::Harness(_) => AdapterError::Harness(text),
            AdapterError::UnknownRequest(_) | AdapterError::Other(_) => AdapterError::Other(text),
            // Kinds without a text of their own keep their kind in the words.
            AdapterError::Unsupported(capability) => AdapterError::Other(format!(
                "{UNSUPPORTED_PREFIX}{capability}\nstderr: {excerpt}"
            )),
            AdapterError::Closed | AdapterError::TurnInProgress => AdapterError::Other(text),
        }
    }
}

impl std::fmt::Display for AdapterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}{}", self.prefix(), self.detail())
    }
}

/// Context of [`HarnessAdapter::commands`].
#[derive(Debug, Clone)]
pub struct CommandContext {
    pub cwd: PathBuf,
    pub native_session_id: Option<String>,
    /// The user's trust decision for the project and this harness (`Project.harnessTrust`), as
    /// [`StartOptions::project_trusted`] passes it to a start: a harness with the feature
    /// `projectTrust` that lists commands without a running agent lists them as an agent
    /// started with this decision would (`None`: not decided, the harness's own saved decision
    /// applies).
    pub project_trusted: Option<bool>,
}

/// A native session found on disk / through the CLI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeSessionSummary {
    pub native_session_id: String,
    pub title: Option<String>,
    pub updated_at: Option<Millis>,
    pub cwd: Option<String>,
}

/// Native sessions with each `native_session_id` once.
///
/// A native session is one conversation, and a thread imports one native session, so a listing
/// names each session once (`native/list` promises clients unique `nativeSessionId`s). Some CLIs
/// list a session several times — Codex's `thread/list` has one entry per rollout file of a
/// resumed thread, with the same id. For each id this keeps the position of its first entry
/// and the content of its entry with the latest `updated_at` (an entry with a time is later
/// than one without; on a tie the earlier entry stays).
#[derive(Debug, Default)]
pub struct NativeSessionSet {
    sessions: Vec<NativeSessionSummary>,
    positions: std::collections::HashMap<String, usize>,
    repeated: usize,
}

impl NativeSessionSet {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds `session`, or merges it into the entry of the same id. Returns whether the id is
    /// new.
    pub fn insert(&mut self, session: NativeSessionSummary) -> bool {
        match self.positions.get(&session.native_session_id) {
            Some(&at) => {
                self.repeated += 1;
                let kept = &mut self.sessions[at];
                if session.updated_at > kept.updated_at {
                    *kept = session;
                }
                false
            }
            None => {
                self.positions
                    .insert(session.native_session_id.clone(), self.sessions.len());
                self.sessions.push(session);
                true
            }
        }
    }

    /// Number of distinct sessions.
    pub fn len(&self) -> usize {
        self.sessions.len()
    }

    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }

    /// Number of entries that repeated an id already present (and were merged into it).
    pub fn repeated(&self) -> usize {
        self.repeated
    }

    pub fn into_sessions(self) -> Vec<NativeSessionSummary> {
        self.sessions
    }
}

impl FromIterator<NativeSessionSummary> for NativeSessionSet {
    fn from_iter<I: IntoIterator<Item = NativeSessionSummary>>(iter: I) -> Self {
        let mut set = Self::new();
        for session in iter {
            set.insert(session);
        }
        set
    }
}

/// A native session that exists but could not be read (e.g. a transcript file that cannot be
/// opened). Listings skip it and report it instead of failing as a whole.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnreadableNativeSession {
    /// Where the session is: its transcript file, or what else identifies it.
    pub location: String,
    pub error: String,
}

/// Result of [`HarnessAdapter::scan_native_sessions`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NativeSessionScan {
    pub sessions: Vec<NativeSessionSummary>,
    pub unreadable: Vec<UnreadableNativeSession>,
}

impl NativeSessionScan {
    /// The readable sessions, after logging every unreadable one with its location (for
    /// [`HarnessAdapter::list_native_sessions`], which has no other way to report them).
    pub fn into_logged_sessions(self, harness_id: &str) -> Vec<NativeSessionSummary> {
        for skipped in &self.unreadable {
            tracing::warn!(
                harness = harness_id,
                location = %skipped.location,
                error = %skipped.error,
                "skipped a native session that cannot be read"
            );
        }
        self.sessions
    }
}

/// History of a native session, for importing it as a thread.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct NativeHistory {
    pub title: Option<String>,
    pub turns: Vec<HistoryTurn>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct HistoryTurn {
    pub started_at: Option<Millis>,
    pub completed_at: Option<Millis>,
    /// Items in order, including the user's message.
    pub items: Vec<HistoryItem>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct HistoryItem {
    pub body: ItemBody,
    pub status: ItemStatus,
}

/// A configured harness.
#[async_trait]
pub trait HarnessAdapter: Send + Sync + 'static {
    fn id(&self) -> &str;
    fn kind(&self) -> HarnessKind;
    fn display_name(&self) -> &str;

    /// Availability, version, capabilities, models and modes. Never fails: problems are
    /// reported through [`HarnessInfo::unavailable`].
    async fn probe(&self) -> HarnessInfo;

    /// Spawns the CLI and completes its handshake.
    async fn start(&self, req: StartRequest) -> Result<SessionHandle, AdapterError>;

    /// What the engine calls to start a session: [`start`](Self::start) with the
    /// [`StartOptions`] of the adapter's features. The default ignores the options (the engine
    /// sets none that [`features`](Self::features) does not offer); adapters with features
    /// that need them at the start override it.
    async fn start_with(
        &self,
        req: StartRequest,
        options: StartOptions,
    ) -> Result<SessionHandle, AdapterError> {
        let _ = options;
        self.start(req).await
    }

    /// Features beyond [`HarnessInfo`] (see the crate docs, "Features beyond the
    /// capabilities"). The engine reads them right after every probe, so they may follow what
    /// the probe learned (e.g. which models support fast mode). Default: none.
    fn features(&self) -> HarnessFeatures {
        HarnessFeatures::default()
    }

    /// Whether a start with [`StartOptions::fork_at`] `point` could branch the source (feature
    /// `forkAtTurn`), judged from the point alone, before anything is started: the checks the
    /// adapter's start makes of the point itself (the anchor is one of this adapter's, the
    /// anchors the cut needs are there). The engine asks before it creates a fork thread, so
    /// that a point the start would always refuse is refused at `thread/fork` instead of
    /// leaving a thread whose every start fails. What only the CLI can tell (the anchor is no
    /// longer in the session) is still found out by the start. Default: every point.
    fn check_fork_point(&self, point: &ForkPoint) -> Result<(), AdapterError> {
        let _ = point;
        Ok(())
    }

    /// Settings in a form an earlier version of this adapter offered and that it now expresses
    /// otherwise (threads and project defaults outlive adapter versions), for settings that
    /// have not been stored with a thread yet (`thread/create`, from the request or the
    /// project's defaults). Returns the settings to use instead and whether they ask for plan
    /// mode ([`ThreadModes::plan`]); e.g. Claude Code's permission mode `plan`, once offered
    /// as a permission mode, is plan mode now. Settings already stored with a thread reach the
    /// adapter unchanged, which handles the old form itself. Default: unchanged.
    fn upgrade_settings(&self, settings: ThreadSettings) -> UpgradedSettings {
        UpgradedSettings {
            settings,
            plan: false,
        }
    }

    /// Harness-native commands for the composer's `/` menu. An adapter may list a command's
    /// aliases as commands of their own (the harness resolves them).
    async fn commands(&self, ctx: CommandContext) -> Result<Vec<Command>, AdapterError>;

    /// Names of harness commands (from [`commands`](Self::commands) and
    /// [`AdapterEvent::CommandsChanged`]) whose effect is to switch the native session inside
    /// the running process: start a new one, open another one, or move to another branch of
    /// it. A thread is one native session, and a switch under a running thread would leave its
    /// history, turns, diffs and interactions describing a conversation the agent no longer has.
    /// Each adapter lists its CLI's commands by explicit name, with the reason; the default is
    /// none. See [`session_switching_names`](Self::session_switching_names).
    fn session_switching_commands(&self) -> &'static [&'static str] {
        &[]
    }

    /// Every name under which the harness runs a session-switching command: the names of
    /// [`session_switching_commands`](Self::session_switching_commands) and their aliases as
    /// the harness lists them (e.g. Claude Code's `clear` with `reset` and `new`, read from its
    /// command list). The engine never offers them in `command/list` and refuses input whose
    /// first word is `/` and one of them (`sessionSwitchingCommand`). The default is the static
    /// names.
    fn session_switching_names(&self) -> Vec<String> {
        self.session_switching_commands()
            .iter()
            .map(|name| (*name).to_owned())
            .collect()
    }

    /// The harness's own status without a session (feature `status`): what it reports for the
    /// account or the CLI in `cwd` (e.g. rate limits). Used by `thread/harnessStatus` when the
    /// thread's agent is not running ([`SessionControl::status`] otherwise). Default: nothing.
    async fn status(&self, cwd: &Path) -> Result<Vec<StatusSection>, AdapterError> {
        let _ = cwd;
        Ok(Vec::new())
    }

    /// Native sessions whose working directory is `cwd` (capability `nativeSessions`), each
    /// `native_session_id` once: a CLI that lists a session several times is merged by the
    /// adapter ([`NativeSessionSet`]). The engine enforces the same rule on every result and
    /// logs a warning when an adapter repeats an id (an adapter defect).
    /// Sessions that exist but cannot be read are skipped and logged with their location;
    /// [`scan_native_sessions`](Self::scan_native_sessions) returns them to the caller.
    async fn list_native_sessions(
        &self,
        cwd: &Path,
    ) -> Result<Vec<NativeSessionSummary>, AdapterError>;

    /// Like [`list_native_sessions`](Self::list_native_sessions), and also returns the sessions
    /// that exist but could not be read, so a caller can show them. An error means the listing
    /// as a whole failed (e.g. the CLI's session store cannot be read at all). The default
    /// suits adapters whose listing is a single request to the CLI, which succeeds or fails
    /// as a whole; adapters that read session files override it.
    async fn scan_native_sessions(&self, cwd: &Path) -> Result<NativeSessionScan, AdapterError> {
        Ok(NativeSessionScan {
            sessions: self.list_native_sessions(cwd).await?,
            unreadable: Vec::new(),
        })
    }

    /// Full history of a native session (capability `nativeSessions`).
    async fn read_native_history(
        &self,
        cwd: &Path,
        native_session_id: &str,
    ) -> Result<NativeHistory, AdapterError>;

    /// [`read_native_history`](Self::read_native_history) with the anchor of each turn (same
    /// order; `None` or missing entries for turns without one), for adapters whose CLI's history
    /// carries the explicit anchors of [`AdapterEvent::TurnAnchor`]. Default: no anchors.
    async fn read_native_history_anchored(
        &self,
        cwd: &Path,
        native_session_id: &str,
    ) -> Result<(NativeHistory, Vec<Option<Value>>), AdapterError> {
        Ok((
            self.read_native_history(cwd, native_session_id).await?,
            Vec::new(),
        ))
    }
}

/// Control of one running session. All methods may be called from any task; the engine
/// never overlaps calls for the same session, except [`status`](Self::status) and
/// [`side_question`](Self::side_question), which change nothing and may run beside the others.
#[async_trait]
pub trait SessionControl: Send + Sync {
    /// Starts a new turn (only while no turn runs).
    async fn send(&self, input: TurnInput) -> Result<(), AdapterError>;
    /// Injects input into the running turn (capability `steer`).
    async fn steer(&self, input: TurnInput) -> Result<(), AdapterError>;
    /// Asks the CLI to stop the running turn; completion arrives as `TurnCompleted`. Returns
    /// within `stop_grace` even when the CLI does not answer or read (an error then); the
    /// engine's forced stop (`interrupt_grace`) runs from the user's request and bounds this
    /// call as well.
    async fn interrupt(&self) -> Result<(), AdapterError>;
    /// Answers an `InteractionRequested`.
    async fn respond(
        &self,
        request_id: &str,
        resolution: &InteractionResolution,
    ) -> Result<(), AdapterError>;
    async fn apply_settings(
        &self,
        settings: &ThreadSettings,
    ) -> Result<SettingsApplied, AdapterError>;
    /// Staged stop (protocol-level cancel when needed → close stdin → grace → terminate the
    /// tree). Idempotent; returns how the process ended.
    async fn shutdown(&self, reason: StopReason) -> ExitInfo;

    /// Asks the harness to stop background task `key` (capability `backgroundStop`). `Ok`
    /// means the request was accepted, not that the task has stopped: its end arrives as an
    /// [`AdapterEvent::BackgroundTask`] in a terminal state (or with `Exited`). Bounded by
    /// `handshake_timeout` like the other requests.
    async fn stop_background(&self, key: &str) -> Result<(), AdapterError> {
        let _ = key;
        Err(AdapterError::Unsupported("backgroundStop"))
    }

    /// The engine expired request `request_id` while the process lives (`reason`: its turn or
    /// its background task ended) and no longer waits for an answer: answer the CLI so that it
    /// does not wait either. [`AdapterError::UnknownRequest`] when the request is not pending
    /// any more (answered, withdrawn). The default answers like a dismissal
    /// ([`InteractionResolution::Dismissed`], which every adapter maps to its CLI's decline or
    /// cancel); adapters whose CLI has a dedicated answer for this override it.
    async fn expire_request(
        &self,
        request_id: &str,
        reason: ExpireReason,
    ) -> Result<(), AdapterError> {
        let _ = reason;
        self.respond(request_id, &InteractionResolution::Dismissed)
            .await
    }

    /// Like [`steer`](Self::steer), naming the engine's id of the steered message
    /// (`message_id`). An adapter whose CLI may not take a steer into the running turn (it
    /// takes it only at a point of its own) withdraws a steer that was not taken and reports
    /// [`AdapterEvent::SteerReturned`] with that id; the engine then queues the message again.
    /// The default steers without an id.
    async fn steer_message(&self, message_id: &str, input: TurnInput) -> Result<(), AdapterError> {
        let _ = message_id;
        self.steer(input).await
    }

    /// Brings the session to `modes` (plan mode, fast mode; features `planMode`,
    /// `fastModeModels`), like [`apply_settings`](Self::apply_settings). Only called while no
    /// turn runs, and only with modes the adapter's features offer. Default: unsupported.
    async fn apply_modes(&self, modes: &ThreadModes) -> Result<SettingsApplied, AdapterError> {
        let _ = modes;
        Err(AdapterError::Unsupported("modes"))
    }

    /// Gives the native session the user's title (feature `rename`). The CLI's echo of it (a
    /// `SessionTitle`) changes nothing. Default: unsupported.
    async fn rename(&self, title: &str) -> Result<(), AdapterError> {
        let _ = title;
        Err(AdapterError::Unsupported("rename"))
    }

    /// The session's own status, in the harness's sections and words (feature `status`). It
    /// changes nothing, so the engine may call it while other calls of the session run.
    /// Bounded by `handshake_timeout`. Default: nothing.
    async fn status(&self) -> Result<Vec<StatusSection>, AdapterError> {
        Ok(Vec::new())
    }

    /// Asks a question beside the conversation (feature `sideQuestion`, Claude Code's `/btw`):
    /// the answer is not part of the session's history. It changes nothing, so the engine may
    /// call it while other calls of the session run (a turn may be running). Default:
    /// unsupported.
    async fn side_question(&self, question: &str) -> Result<SideAnswer, AdapterError> {
        let _ = question;
        Err(AdapterError::Unsupported("sideQuestion"))
    }

    /// Moves the running item `item_key` to the background (feature `moveToBackground`); only
    /// called for an item the adapter reported backgroundable
    /// ([`AdapterEvent::ItemBackgroundable`]). `Ok` means the CLI accepted the request: the item
    /// then closes as `Backgrounded` with its task, as for work started in the background.
    /// Default: unsupported.
    async fn move_to_background(&self, item_key: &str) -> Result<(), AdapterError> {
        let _ = item_key;
        Err(AdapterError::Unsupported("moveToBackground"))
    }
}

type StopFn = Box<dyn FnOnce(StopReason) -> Pin<Box<dyn Future<Output = ExitInfo> + Send>> + Send>;

/// Stops a process whose session start did not complete.
///
/// An adapter's `start` (and its probes) spawn the CLI and then await a handshake. The caller
/// may drop that future at any await point — the engine when a thread is stopped while
/// starting, a transport when a request is cancelled. Reader tasks keep the process handle
/// alive, so nothing else would ever stop it. Arm a guard right after spawning; call
/// [`disarm`](Self::disarm) once the session is handed over, or [`stop`](Self::stop) to stop it
/// on an error path. Dropped while armed, the guard stops the process in a background task
/// ([`StopReason::Abandoned`]).
pub struct StartGuard {
    stop: Option<StopFn>,
}

impl StartGuard {
    /// `stop` performs the staged stop and reports how the process ended.
    pub fn new<F, Fut>(stop: F) -> Self
    where
        F: FnOnce(StopReason) -> Fut + Send + 'static,
        Fut: Future<Output = ExitInfo> + Send + 'static,
    {
        Self {
            stop: Some(Box::new(move |reason| Box::pin(stop(reason)))),
        }
    }

    /// Guards a session through its [`SessionControl::shutdown`].
    pub fn for_session(session: Arc<dyn SessionControl>) -> Self {
        Self::new(move |reason| async move { session.shutdown(reason).await })
    }

    /// The session was handed over: nothing to stop.
    pub fn disarm(mut self) {
        self.stop = None;
    }

    /// Stops the process now. The stop runs in its own task, so dropping the caller cannot
    /// interrupt it half-way.
    pub async fn stop(mut self, reason: StopReason) -> ExitInfo {
        let stop = self
            .stop
            .take()
            .expect("an armed guard holds its stop function");
        match tokio::spawn(stop(reason)).await {
            Ok(info) => info,
            Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
            Err(_) => ExitInfo {
                code: None,
                stopped: Some(reason),
                stderr_tail: String::new(),
                exited_at_ms: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or(0),
            },
        }
    }
}

impl Drop for StartGuard {
    fn drop(&mut self) {
        let Some(stop) = self.stop.take() else { return };
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => {
                tracing::info!("session start abandoned; stopping its process");
                runtime.spawn(stop(StopReason::Abandoned));
            }
            // Without a runtime the process handles are dropped with their owners, which
            // makes the supervisor terminate the tree (`StopReason::Abandoned`).
            Err(_) => tracing::warn!(
                "session start abandoned outside a runtime; the process is left to its handles"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A session that records its shutdown calls.
    #[derive(Default)]
    struct Recorder {
        stops: std::sync::Mutex<Vec<StopReason>>,
    }

    #[async_trait]
    impl SessionControl for Recorder {
        async fn send(&self, _input: TurnInput) -> Result<(), AdapterError> {
            Ok(())
        }
        async fn steer(&self, _input: TurnInput) -> Result<(), AdapterError> {
            Ok(())
        }
        async fn interrupt(&self) -> Result<(), AdapterError> {
            Ok(())
        }
        async fn respond(
            &self,
            _request_id: &str,
            _resolution: &InteractionResolution,
        ) -> Result<(), AdapterError> {
            Ok(())
        }
        async fn apply_settings(
            &self,
            _settings: &ThreadSettings,
        ) -> Result<SettingsApplied, AdapterError> {
            Ok(SettingsApplied::Live)
        }
        async fn shutdown(&self, reason: StopReason) -> ExitInfo {
            self.stops.lock().unwrap().push(reason);
            ExitInfo {
                code: Some(0),
                stopped: Some(reason),
                stderr_tail: String::new(),
                exited_at_ms: 0,
            }
        }
    }

    #[tokio::test]
    async fn a_dropped_start_guard_stops_the_session() {
        let session = Arc::new(Recorder::default());
        // A start future dropped half-way through its handshake.
        let start = {
            let session = session.clone();
            async move {
                let _guard = StartGuard::for_session(session);
                std::future::pending::<()>().await;
            }
        };
        assert!(
            tokio::time::timeout(Duration::from_millis(10), start)
                .await
                .is_err()
        );
        for _ in 0..100 {
            if !session.stops.lock().unwrap().is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(*session.stops.lock().unwrap(), vec![StopReason::Abandoned]);
    }

    #[tokio::test]
    async fn a_disarmed_guard_does_nothing_and_stop_reports_the_exit() {
        let session = Arc::new(Recorder::default());
        StartGuard::for_session(session.clone()).disarm();
        tokio::task::yield_now().await;
        assert!(session.stops.lock().unwrap().is_empty());
        let info = StartGuard::for_session(session.clone())
            .stop(StopReason::Shutdown)
            .await;
        assert_eq!(info.stopped, Some(StopReason::Shutdown));
        assert_eq!(*session.stops.lock().unwrap(), vec![StopReason::Shutdown]);
    }

    fn native(id: &str, title: &str, updated_at: Option<Millis>) -> NativeSessionSummary {
        NativeSessionSummary {
            native_session_id: id.into(),
            title: Some(title.into()),
            updated_at,
            cwd: None,
        }
    }

    #[test]
    fn a_native_session_set_keeps_each_id_once_with_its_latest_entry() {
        let set: NativeSessionSet = [
            native("a", "a (older rollout)", Some(10)),
            native("b", "b", Some(30)),
            // Later than the first entry of `a`: its content wins, the position stays.
            native("a", "a (latest rollout)", Some(40)),
            // Older than the kept entry: ignored.
            native("a", "a (oldest rollout)", Some(5)),
            native("c", "c", None),
            // An entry with a time is later than one without.
            native("c", "c (timed)", Some(1)),
            // A tie keeps the earlier entry.
            native("b", "b (tie)", Some(30)),
        ]
        .into_iter()
        .collect();
        assert_eq!(set.len(), 3);
        assert_eq!(set.repeated(), 4);
        assert_eq!(
            set.into_sessions(),
            vec![
                native("a", "a (latest rollout)", Some(40)),
                native("b", "b", Some(30)),
                native("c", "c (timed)", Some(1)),
            ]
        );
        let mut set = NativeSessionSet::new();
        assert!(set.is_empty());
        assert!(set.insert(native("x", "x", None)));
        assert!(!set.insert(native("x", "x", None)));
        assert_eq!((set.len(), set.repeated()), (1, 1));
    }

    #[test]
    fn titles_follow_the_policy() {
        let policy = AdapterPolicy {
            first_message_title_chars: 5,
            harness_title_chars: 4,
            ..AdapterPolicy::default()
        };
        assert_eq!(
            policy.prompt_title("\n  hello world  \nsecond").as_deref(),
            Some("hello…")
        );
        assert_eq!(policy.prompt_title("  hi \n").as_deref(), Some("hi"));
        assert_eq!(policy.prompt_title(" \n \n"), None);
        assert_eq!(policy.harness_title("  named  ").as_deref(), Some("name"));
        assert_eq!(policy.harness_title("   "), None);
        // Characters, not bytes.
        assert_eq!(title_from_first_line("日本語のタイトル", 3), "日本語…");
        assert_eq!(harness_title("日本語のタイトル", 3), "日本語");
        let defaults = AdapterPolicy::default();
        assert_eq!(
            (
                defaults.first_message_title_chars,
                defaults.harness_title_chars
            ),
            (80, 200)
        );
    }

    fn agent(key: &str) -> BackgroundTaskInfo {
        BackgroundTaskInfo::new(key, BackgroundTaskKind::Agent, format!("task {key}"))
    }

    #[test]
    fn a_task_that_starts_again_after_its_end_is_a_new_run() {
        let mut tasks = BackgroundTasks::new();
        let first = tasks
            .started(BackgroundTaskInfo {
                origin_item_key: Some("tool1".into()),
                ..agent("a")
            })
            .unwrap();
        assert_eq!((first.runs, first.state), (1, BackgroundState::Running));
        // A repeated start of the running task adds nothing new: no event.
        assert_eq!(tasks.started(agent("a")), None);
        let done = tasks
            .ended(
                "a",
                BackgroundState::Completed,
                Some(BackgroundOutcome {
                    summary: Some("done".into()),
                    ..Default::default()
                }),
            )
            .unwrap();
        assert_eq!(done.state, BackgroundState::Completed);
        assert_eq!(
            done.result.as_ref().unwrap().summary.as_deref(),
            Some("done")
        );
        let again = tasks.started(agent("a")).unwrap();
        assert_eq!((again.runs, again.state), (2, BackgroundState::Running));
        assert_eq!(again.result, None, "a new run has no result yet");
        assert_eq!(
            again.origin_item_key.as_deref(),
            Some("tool1"),
            "the origin stays"
        );
        assert_eq!(
            tasks.ended("unknown", BackgroundState::Failed, None),
            None,
            "unknown keys are left alone"
        );
    }

    #[test]
    fn the_live_set_replaces_itself_and_is_kept_by_later_starts() {
        let mut tasks = BackgroundTasks::new();
        tasks.started(agent("a"));
        // The level report may name a task before its start signal.
        let changed = tasks.replace_live(
            [
                LiveEntry {
                    key: "a".into(),
                    ambient: false,
                },
                LiveEntry {
                    key: "m".into(),
                    ambient: true,
                },
            ],
            |e| BackgroundTaskInfo::new(e.key.clone(), BackgroundTaskKind::Monitor, "monitor"),
        );
        assert_eq!(changed.len(), 1, "only the new entry changed: {changed:?}");
        assert!(changed[0].live && changed[0].ambient);
        assert!(tasks.any_busy());
        // The start of a known task keeps what the level said.
        tasks.started(BackgroundTaskInfo {
            live: false,
            ..agent("a")
        });
        assert!(tasks.get("a").unwrap().live);
        // A report without `a`: it is not live any more (still running as far as the edges
        // say); the ambient monitor never keeps the session busy.
        let changed = tasks.replace_live(
            [LiveEntry {
                key: "m".into(),
                ambient: true,
            }],
            |_| unreachable!(),
        );
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0].key, "a");
        assert!(!changed[0].live && changed[0].state == BackgroundState::Running);
        assert!(!tasks.any_busy());
        assert!(tasks.replace_live([], |_| unreachable!()).len() == 1);
        assert!(tasks.iter().all(|t| !t.live));
    }

    #[tokio::test]
    async fn an_expired_request_is_answered_like_a_dismissal_by_default() {
        #[derive(Default)]
        struct Answers(std::sync::Mutex<Vec<(String, InteractionResolution)>>);
        #[async_trait]
        impl SessionControl for Answers {
            async fn send(&self, _input: TurnInput) -> Result<(), AdapterError> {
                Ok(())
            }
            async fn steer(&self, _input: TurnInput) -> Result<(), AdapterError> {
                Ok(())
            }
            async fn interrupt(&self) -> Result<(), AdapterError> {
                Ok(())
            }
            async fn respond(
                &self,
                request_id: &str,
                resolution: &InteractionResolution,
            ) -> Result<(), AdapterError> {
                self.0
                    .lock()
                    .unwrap()
                    .push((request_id.to_owned(), resolution.clone()));
                Ok(())
            }
            async fn apply_settings(
                &self,
                _settings: &ThreadSettings,
            ) -> Result<SettingsApplied, AdapterError> {
                Ok(SettingsApplied::Live)
            }
            async fn shutdown(&self, reason: StopReason) -> ExitInfo {
                ExitInfo {
                    code: Some(0),
                    stopped: Some(reason),
                    stderr_tail: String::new(),
                    exited_at_ms: 0,
                }
            }
        }
        let session = Answers::default();
        session
            .expire_request("r1", ExpireReason::TaskEnded)
            .await
            .unwrap();
        assert_eq!(
            *session.0.lock().unwrap(),
            vec![("r1".to_owned(), InteractionResolution::Dismissed)]
        );
        assert_eq!(
            session.stop_background("k").await,
            Err(AdapterError::Unsupported("backgroundStop"))
        );
    }

    #[test]
    fn terminal_control_is_removed_and_the_text_kept() {
        let colored = "\u{1b}[1;31merror\u{1b}[0m: session \u{1b}[4mabc\u{1b}[24m is held";
        assert_eq!(
            sanitize_terminal_text(colored),
            "error: session abc is held"
        );
        // OSC (window title, hyperlinks) ended by BEL or ST, C1 CSI, charset selection (nF),
        // keypad mode (Fp), cursor save (Fe), lone control characters.
        let mixed = "\u{1b}]0;title\u{07}a\u{1b}]8;;http://x\u{1b}\\link\u{1b}]8;;\u{1b}\\\u{9b}2Kb\u{1b}(Bc\u{1b}=d\u{1b}7e\u{8}\u{0}f";
        assert_eq!(sanitize_terminal_text(mixed), "alinkbcdef");
        // DCS and APC strings are dropped whole.
        assert_eq!(
            sanitize_terminal_text("x\u{1b}Pq#0;2;0;0;0\u{1b}\\y\u{1b}_apc\u{1b}\\z"),
            "xyz"
        );
        // Line breaks become `\n`; tabs stay; text of other scripts is untouched.
        assert_eq!(
            sanitize_terminal_text("一行目\r\n二行目\r三行目\tend"),
            "一行目\n二行目\n三行目\tend"
        );
        // A sequence cut off at the end of the text leaves nothing behind.
        assert_eq!(sanitize_terminal_text("done\u{1b}[3"), "done");
        assert_eq!(sanitize_terminal_text("done\u{1b}"), "done");
    }

    #[test]
    fn stderr_excerpts_keep_the_last_lines_without_escapes() {
        let tail = "\n\u{1b}[2mstarting\u{1b}[0m\none\ntwo\n\u{1b}[31mthree\u{1b}[0m\n\n";
        assert_eq!(stderr_excerpt(tail, 2), "two\nthree");
        assert_eq!(stderr_excerpt(tail, 0), "");
        assert_eq!(stderr_excerpt("  \n\u{1b}[0m\n", 5), "");
    }

    #[test]
    fn adapter_errors_never_repeat_their_prefix() {
        let inner = AdapterError::Harness("initialize: \u{1b}[31mnot logged in\u{1b}[0m".into());
        assert_eq!(
            inner.to_string(),
            "the harness reported an error: initialize: not logged in"
        );
        assert_eq!(inner.detail(), "initialize: not logged in");
        // An adapter that formats an error into another one gets each prefix once.
        let wrapped = AdapterError::Harness(format!("{inner}; claude stderr: boom"));
        assert_eq!(
            wrapped.to_string(),
            "the harness reported an error: initialize: not logged in; claude stderr: boom"
        );
        let spawned = AdapterError::Spawn(format!("{wrapped}"));
        assert_eq!(
            spawned.to_string(),
            "failed to start the harness: initialize: not logged in; claude stderr: boom"
        );
        assert_eq!(AdapterError::Unsupported("fork").detail(), "fork");
        assert_eq!(AdapterError::Closed.to_string(), "the session is closed");
    }

    #[test]
    fn stderr_is_added_to_the_text_of_the_same_kind() {
        let policy = AdapterPolicy {
            stderr_excerpt_lines: 1,
            ..AdapterPolicy::default()
        };
        let e = policy.with_stderr(
            AdapterError::Harness("the harness reported an error: resume failed".into()),
            "noise\n\u{1b}[31mError: session is held\u{1b}[0m\n",
        );
        assert_eq!(
            e,
            AdapterError::Harness("resume failed\nstderr: Error: session is held".into())
        );
        assert_eq!(
            e.to_string(),
            "the harness reported an error: resume failed\nstderr: Error: session is held"
        );
        // Nothing on stderr: unchanged.
        let same = AdapterError::Spawn("x".into());
        assert_eq!(policy.with_stderr(same.clone(), " \n"), same);
        assert!(matches!(
            AdapterError::Unsupported("fork").with_stderr("why", 5),
            AdapterError::Other(m) if m == "not supported by this harness: fork\nstderr: why"
        ));
    }

    #[tokio::test]
    async fn the_extended_port_methods_refuse_or_report_nothing_by_default() {
        let session = Recorder::default();
        assert_eq!(
            session.apply_modes(&ThreadModes::default()).await,
            Err(AdapterError::Unsupported("modes"))
        );
        assert_eq!(
            session.rename("t").await,
            Err(AdapterError::Unsupported("rename"))
        );
        assert_eq!(session.status().await, Ok(Vec::new()));
        assert_eq!(
            session.side_question("q").await,
            Err(AdapterError::Unsupported("sideQuestion"))
        );
        assert_eq!(
            session.move_to_background("k").await,
            Err(AdapterError::Unsupported("moveToBackground"))
        );
        // A steer with the engine's id is a plain steer.
        assert_eq!(
            session.steer_message("itm_1", TurnInput::text("x")).await,
            Ok(())
        );
    }

    #[test]
    fn a_fork_point_round_trips_as_json() {
        let point = ForkPoint {
            anchor: serde_json::json!({"turnId": "t2"}),
            before: true,
            previous: None,
        };
        let json = serde_json::to_value(&point).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"anchor": {"turnId": "t2"}, "before": true})
        );
        assert_eq!(serde_json::from_value::<ForkPoint>(json).unwrap(), point);
    }

    #[test]
    fn plain_text_rendering_inserts_mentions() {
        let input = TurnInput {
            parts: vec![
                TurnInputPart::Text("look at".into()),
                TurnInputPart::Mention {
                    relative: "src/main.rs".into(),
                    absolute: PathBuf::from("/x/src/main.rs"),
                },
                TurnInputPart::Image {
                    path: PathBuf::from("/tmp/a.png"),
                    mime: "image/png".into(),
                },
                TurnInputPart::Text(" please".into()),
            ],
        };
        assert_eq!(input.to_plain_text(), "look at @src/main.rs please");
        assert_eq!(input.images().count(), 1);
    }
}
