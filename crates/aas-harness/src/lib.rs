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
//!   only while one runs and only when [`HarnessCapabilities::steer`] is set. The engine
//!   serializes all calls of one session.
//! * A turn ends with exactly one [`AdapterEvent::TurnCompleted`]. Items still open at that
//!   point are closed by the engine.
//! * [`AdapterEvent::TurnStarted`] may also arrive without a preceding `send` when the CLI
//!   starts a run by itself (hooks, extensions, background notifications); the engine then
//!   records an agent-initiated turn.
//! * When the process dies mid-turn an adapter may emit `TurnCompleted { status: Failed }`
//!   before `Exited`, or only `Exited`; the engine fails a turn that is still open.
//! * [`AdapterEvent::SessionInfo`] reports what the CLI resolved (e.g. a full model id); the
//!   engine records it on turns but never rewrites the thread's settings with it.
//! * Every child process is spawned through [`AdapterContext::supervisor`].
//! * Anything the adapter cannot map is forwarded as [`AdapterEvent::Native`] instead of
//!   being dropped or guessed.

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
    Command, ContextUsage, DeltaField, EffortLevel, HarnessCapabilities, HarnessKind,
    InteractionRequest, InteractionResolution, ItemBody, ItemStatus, Millis, Model, NoticeLevel,
    PermissionMode, ThreadId, ThreadSettings, TurnError, TurnStatus, Usage,
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
}

impl Default for AdapterPolicy {
    fn default() -> Self {
        Self {
            stop_grace: Duration::from_secs(5),
            max_line_bytes: 64 * 1024 * 1024,
            handshake_timeout: Duration::from_secs(60),
        }
    }
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

/// Normalized events emitted by a session. Items are identified by an adapter-chosen `key`
/// (unique within the session); the engine maps keys to protocol item ids.
#[derive(Debug, Clone, PartialEq)]
pub enum AdapterEvent {
    /// The native session id became known or changed (e.g. a fork got its own id).
    SessionIdentified {
        native_session_id: String,
    },
    /// Model / permission mode / effort the CLI reports as current.
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
    },
    /// Shown to the user as a notice item.
    Notice {
        level: NoticeLevel,
        message: String,
        code: Option<String>,
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
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AdapterError {
    #[error("harness unavailable: {0}")]
    Unavailable(String),
    #[error("not supported by this harness: {0}")]
    Unsupported(&'static str),
    #[error("failed to start the harness: {0}")]
    Spawn(String),
    #[error("unexpected message from the harness: {0}")]
    Protocol(String),
    #[error("the harness reported an error: {0}")]
    Harness(String),
    #[error("the session is closed")]
    Closed,
    #[error("unknown request id {0}")]
    UnknownRequest(String),
    #[error("{0}")]
    Other(String),
}

/// Context of [`HarnessAdapter::commands`].
#[derive(Debug, Clone)]
pub struct CommandContext {
    pub cwd: PathBuf,
    pub native_session_id: Option<String>,
}

/// A native session found on disk / through the CLI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeSessionSummary {
    pub native_session_id: String,
    pub title: Option<String>,
    pub updated_at: Option<Millis>,
    pub cwd: Option<String>,
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

    /// Harness-native commands for the composer's `/` menu.
    async fn commands(&self, ctx: CommandContext) -> Result<Vec<Command>, AdapterError>;

    /// Native sessions whose working directory is `cwd` (capability `nativeSessions`).
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
}

/// Control of one running session. All methods may be called from any task; the engine
/// never overlaps calls for the same session.
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
