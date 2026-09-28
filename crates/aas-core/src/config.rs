//! Policy values, heuristic thresholds and engine configuration.

use std::path::{Path, PathBuf};
use std::time::Duration;

use aas_eventlog::BatchLimits;
use aas_harness::AdapterPolicy;
use aas_protocol::ClientPolicy;
use aas_supervisor::SupervisorPolicy;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// Policy values (`[policy]` in config.toml). Timeouts and sizes live here, never as magic
/// numbers in code. Defaults, their rationale and the lower bound of every value:
/// docs/design.md §13.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Policy {
    /// Server → client heartbeat interval. Well below the shortest carrier NAT idle timeouts.
    #[serde(with = "humantime_serde")]
    pub heartbeat_interval: Duration,
    /// A connection with no inbound frame for this long is considered dead (3 heartbeats).
    #[serde(with = "humantime_serde")]
    pub client_timeout: Duration,
    /// Wait after closing a CLI's stdin before terminating its tree.
    #[serde(with = "humantime_serde")]
    pub stop_grace: Duration,
    /// Wait for a CLI to honour an interrupt before stopping its process.
    #[serde(with = "humantime_serde")]
    pub interrupt_grace: Duration,
    /// When Windows ends the session (sign-out, shutdown, reboot), how long the engine waits
    /// for the agents' staged stop (stdin closed, each CLI saves its session and exits) before
    /// it returns and the daemon exits, which terminates whatever is left through the Job
    /// Objects. It must leave room within the daemon's `end_session_deadline` for closing the
    /// connections and exiting; `stop_grace` (5 s) does not fit into the time Windows gives.
    #[serde(with = "humantime_serde")]
    pub end_session_stop_grace: Duration,
    /// Processes idle this long are stopped; the next input resumes the native session. Idle:
    /// no turn runs, no queued input would start one, and no background task keeps the agent
    /// busy (the harness's live set holds nothing but ambient work). The wait starts when the
    /// process becomes idle; background work the harness reports as running is never stopped
    /// because of time (design.md §4.7, §5.6).
    #[serde(with = "humantime_serde")]
    pub idle_process_ttl: Duration,
    /// Upper bound of concurrently alive agent processes; further starts wait (FIFO).
    pub max_running_processes: usize,
    /// How long results of mutating requests are kept for deduplicating resends.
    #[serde(with = "humantime_serde")]
    pub idempotency_ttl: Duration,
    /// Deltas of completed items older than this are compacted away.
    #[serde(with = "humantime_serde")]
    pub delta_retention: Duration,
    pub max_batch_events: usize,
    pub max_batch_bytes: usize,
    /// Command / tool output beyond this many bytes goes to a blob instead of deltas.
    pub max_inline_output_bytes: usize,
    /// Diffs larger than this are returned as a blob.
    pub max_inline_patch_bytes: usize,
    pub max_blob_bytes: u64,
    /// Upper bound of a request frame (reported to clients; larger requests are answered
    /// with a definitive `payloadTooLarge`).
    pub max_client_frame_bytes: usize,
    /// Upper bound of a frame the transport reads at all. Frames between
    /// `max_client_frame_bytes` and this are still read so that they can be answered with
    /// `payloadTooLarge` (the client then drops the request from its outbox instead of
    /// resending it forever); larger frames close the connection with 1009.
    pub max_transport_frame_bytes: usize,
    #[serde(with = "humantime_serde")]
    pub pairing_code_ttl: Duration,
    /// Pairing attempts (`POST /v1/pair`) the whole daemon accepts per `pairing_rate_window`.
    /// Written `pairing_attempts_per_minute` before the window became a policy value; that
    /// name is still accepted.
    #[serde(alias = "pairing_attempts_per_minute")]
    pub pairing_attempts_per_window: u32,
    /// The fixed window `pairing_attempts_per_window` counts in.
    #[serde(with = "humantime_serde")]
    pub pairing_rate_window: Duration,
    /// Characters kept of the device name an app gives when pairing.
    pub device_name_chars: usize,
    /// Characters kept of the platform an app gives when pairing.
    pub device_platform_chars: usize,
    #[serde(with = "humantime_serde")]
    pub file_index_ttl: Duration,
    /// Upper bound for short-lived tools (git status/diff/worktree …).
    #[serde(with = "humantime_serde")]
    pub tool_timeout: Duration,
    /// Upper bound for `git clone`.
    #[serde(with = "humantime_serde")]
    pub clone_timeout: Duration,
    /// Minimum time between two progress updates of one operation (`operation/updated` with a
    /// new `progress`). git rewrites its progress line many times a second; a phone needs about
    /// one refresh per second, and every update is an event in the workspace log, so the latest
    /// line is published at most this often (lines in between are superseded, never queued).
    #[serde(with = "humantime_serde")]
    pub operation_progress_interval: Duration,
    /// Upper bound of one relayed progress line. Progress lines are about 100 bytes; a longer
    /// line (a remote can print anything) is cut so that one update stays a small event.
    pub max_progress_line_bytes: usize,
    /// Minimum time between two stored updates of a running background task that change only
    /// its progress or usage. A workflow reports the whole list of its agents with every step,
    /// and every update is a `backgroundTask/updated` in the thread's log; the phone needs
    /// about one refresh per second. The latest state is kept and written when the interval
    /// has passed, and any other change (an end, a new run, a stop request) is written at
    /// once with it, so nothing but intermediate progress is left out.
    #[serde(with = "humantime_serde")]
    pub background_progress_interval: Duration,
    /// How long after a `backgroundTask/stop` the task shows as stopping while the harness has
    /// not reported its end. The harness answers a stop within milliseconds when it honours it
    /// (Claude's `stop_task`, measured at about 50 ms); 30 s leaves room for slow tool
    /// teardown. Past it the task is marked `stopUnconfirmedAt` so the phone offers the stop
    /// again; nothing is escalated (`thread/stop` stops everything).
    #[serde(with = "humantime_serde")]
    pub background_stop_confirm_timeout: Duration,
    pub stderr_tail_bytes: usize,
    /// Lines of the end of an agent's stderr quoted in the error of a turn whose agent exited
    /// unexpectedly: enough for the usual last words (an exception and its cause) while the
    /// error stays readable on a phone. The whole tail stays in the daemon's log.
    pub exit_message_stderr_lines: usize,
    /// Characters of a thread title taken from the first line of its first message: fits one
    /// line of the phone's thread list.
    pub first_message_title_chars: usize,
    /// Characters kept of a title the harness gave its session (harnesses write whole
    /// sentences; more than two lines of the list is noise).
    pub harness_title_chars: usize,
    /// Characters of the preview of a queued message (its first line), shown in the queue.
    pub queued_preview_chars: usize,
    /// Keep the PC awake while at least one turn runs or background work keeps an agent busy.
    pub prevent_sleep_while_running: bool,
    /// Period of compaction and garbage collection.
    #[serde(with = "humantime_serde")]
    pub maintenance_interval: Duration,
    /// Deadline of a request to a CLI: handshakes (initialize, session creation, listings) and
    /// the requests of a running session (starting a turn, steering, answering, applying
    /// settings), which the thread actor waits for (design.md §4.3). Also how long a request
    /// that needs an unavailable harness waits for the probe it triggers (design.md §9.4).
    #[serde(with = "humantime_serde")]
    pub handshake_timeout: Duration,
    /// Upper bound of one JSON line read from a CLI.
    pub max_line_bytes: usize,
    /// Wait for a tree to disappear after TerminateJobObject.
    #[serde(with = "humantime_serde")]
    pub kill_confirm_timeout: Duration,
    /// Days of daily log files kept.
    pub log_retention_days: u32,

    // ----- harness availability (design.md §9.4) ---------------------------------------------
    /// A request that needs an unavailable harness (`thread/create`, `turn/start`, and the
    /// `queue/*` requests that start a turn) probes it again before refusing — unless a probe
    /// of it started less than this long ago, whose result is used instead. Requests resent
    /// together from a client's outbox then cost one CLI start, not one each.
    /// `harness/refresh` always probes.
    #[serde(with = "humantime_serde")]
    pub harness_probe_min_interval: Duration,
    /// Wait after a probe that found a harness unavailable before it is probed again by
    /// itself. Covers what goes away on its own: a network that is not up yet at logon, a
    /// cold start that missed the handshake deadline, a CLI the user logs in to later.
    #[serde(with = "humantime_serde")]
    pub harness_retry_initial_delay: Duration,
    /// Upper bound of that wait, which doubles with every further failed probe: a harness
    /// that stays unavailable (not installed, never logged in) costs one probe per this
    /// period, and one that recovers is found within it without any request.
    #[serde(with = "humantime_serde")]
    pub harness_retry_max_delay: Duration,

    // ----- storage ---------------------------------------------------------------------------
    /// How long a connection waits for SQLite's lock before a statement fails. Writes are
    /// serialized on one connection, so the daemon itself rarely waits; this covers another
    /// program (a backup, a virus scanner, `doctor`) briefly holding the database file.
    #[serde(with = "humantime_serde")]
    pub sqlite_busy_timeout: Duration,
    /// Size the WAL file is truncated to after a checkpoint (SQLite's `journal_size_limit`).
    /// Without it a burst of agent output leaves a large `-wal` file behind for good.
    pub sqlite_journal_size_limit: u64,
    /// Attempts (the first one included) to persist a batch of thread changes or another
    /// write that has no client waiting for it, before the daemon fail-stops. Transient
    /// failures (a lock held by another program, a file briefly opened exclusively) clear
    /// within seconds; a write that keeps failing means the event log — the source of truth —
    /// can no longer be kept, and continuing would lose what the agents produce.
    pub storage_retry_attempts: u32,
    /// Wait before the second attempt; it doubles with every further attempt.
    #[serde(with = "humantime_serde")]
    pub storage_retry_initial_backoff: Duration,
    /// Upper bound of the doubling wait between attempts.
    #[serde(with = "humantime_serde")]
    pub storage_retry_max_backoff: Duration,

    // ----- retention -------------------------------------------------------------------------
    /// Blobs no item or queued input refers to (an uploaded image not sent yet, a patch
    /// produced for a download, the blobs of a removed thread) are deleted after this long.
    /// Equal to `idempotency_ttl` by default: a `turn/start` resent from the client's outbox
    /// must still find the image it refers to.
    #[serde(with = "humantime_serde")]
    pub unreferenced_blob_grace: Duration,
    /// Events whose whole content is carried by a later event of the same entity (for
    /// example an older `thread/updated`) are deleted once they are this old.
    #[serde(with = "humantime_serde")]
    pub superseded_event_retention: Duration,
    /// `native` events (raw harness output that no state is built from) are deleted once
    /// they are this old.
    #[serde(with = "humantime_serde")]
    pub native_event_retention: Duration,
    /// Finished operations (clones) are forgotten after this long.
    #[serde(with = "humantime_serde")]
    pub finished_operation_retention: Duration,
    /// Rows one maintenance transaction compacts or deletes at most; maintenance continues
    /// with further transactions, so each one stays short and agent events are not delayed.
    pub maintenance_batch_size: usize,
    /// Free pages one `incremental_vacuum` transaction returns to the file system at most
    /// (maintenance repeats it until nothing is left).
    pub incremental_vacuum_pages: u32,

    // ----- limits ----------------------------------------------------------------------------
    /// Upper bound of `fs/search`'s `limit`.
    pub max_file_search_results: usize,
    /// Revocation notices buffered for the transport; a lagging receiver re-checks every
    /// connected device instead, so this only bounds memory.
    pub revocation_backlog: usize,
    /// `thread/list`: page size when the client gives none, and the largest one accepted.
    pub thread_list_default_limit: usize,
    pub thread_list_max_limit: usize,
    /// `thread/read`: turns returned when the client gives no limit, and the most accepted.
    pub thread_read_default_turns: usize,
    pub thread_read_max_turns: usize,
    /// Operations returned by `operation/list` (newest first).
    pub operation_list_limit: usize,
    /// Operations included in `workspace/snapshot` (newest first).
    pub snapshot_operation_limit: usize,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            heartbeat_interval: Duration::from_secs(15),
            client_timeout: Duration::from_secs(45),
            stop_grace: Duration::from_secs(5),
            interrupt_grace: Duration::from_secs(10),
            end_session_stop_grace: Duration::from_secs(2),
            idle_process_ttl: Duration::from_secs(30 * 60),
            max_running_processes: 4,
            idempotency_ttl: Duration::from_secs(7 * 24 * 3600),
            delta_retention: Duration::from_secs(24 * 3600),
            max_batch_events: 512,
            max_batch_bytes: 256 * 1024,
            max_inline_output_bytes: 64 * 1024,
            max_inline_patch_bytes: 64 * 1024,
            max_blob_bytes: 25 * 1024 * 1024,
            max_client_frame_bytes: 1024 * 1024,
            max_transport_frame_bytes: 16 * 1024 * 1024,
            pairing_code_ttl: Duration::from_secs(5 * 60),
            pairing_attempts_per_window: 10,
            pairing_rate_window: Duration::from_secs(60),
            device_name_chars: 80,
            device_platform_chars: 32,
            file_index_ttl: Duration::from_secs(30),
            tool_timeout: Duration::from_secs(120),
            clone_timeout: Duration::from_secs(30 * 60),
            operation_progress_interval: Duration::from_secs(1),
            max_progress_line_bytes: 1024,
            background_progress_interval: Duration::from_secs(1),
            background_stop_confirm_timeout: Duration::from_secs(30),
            stderr_tail_bytes: 16 * 1024,
            exit_message_stderr_lines: 5,
            first_message_title_chars: 80,
            harness_title_chars: 200,
            queued_preview_chars: 120,
            prevent_sleep_while_running: true,
            maintenance_interval: Duration::from_secs(10 * 60),
            handshake_timeout: Duration::from_secs(60),
            max_line_bytes: 64 * 1024 * 1024,
            kill_confirm_timeout: Duration::from_secs(10),
            log_retention_days: 14,
            harness_probe_min_interval: Duration::from_secs(10),
            harness_retry_initial_delay: Duration::from_secs(30),
            harness_retry_max_delay: Duration::from_secs(15 * 60),
            sqlite_busy_timeout: Duration::from_secs(5),
            sqlite_journal_size_limit: 64 * 1024 * 1024,
            storage_retry_attempts: 5,
            storage_retry_initial_backoff: Duration::from_millis(200),
            storage_retry_max_backoff: Duration::from_secs(5),
            unreferenced_blob_grace: Duration::from_secs(7 * 24 * 3600),
            superseded_event_retention: Duration::from_secs(24 * 3600),
            native_event_retention: Duration::from_secs(24 * 3600),
            finished_operation_retention: Duration::from_secs(7 * 24 * 3600),
            maintenance_batch_size: 1000,
            incremental_vacuum_pages: 1024,
            max_file_search_results: 500,
            revocation_backlog: 16,
            thread_list_default_limit: 50,
            thread_list_max_limit: 500,
            thread_read_default_turns: 20,
            thread_read_max_turns: 200,
            operation_list_limit: 50,
            snapshot_operation_limit: 20,
        }
    }
}

// ----- lower bounds ------------------------------------------------------------------------------

/// The lower bound of timers the daemon arms from a policy value (heartbeats, graces,
/// timeouts, time-to-live values): below it a timer fires before anything it guards can happen,
/// and zero makes periodic timers spin (`tokio::time::interval` even panics). The tests of this
/// project run with timers of a few hundred milliseconds.
pub const MIN_TIMER: Duration = Duration::from_millis(100);
/// The lower bound of the periodic background work of the engine (maintenance) and of the
/// timeouts of external tools, which take longer than a timer tick even when all is well.
pub const MIN_BACKGROUND_PERIOD: Duration = Duration::from_secs(1);
/// The lower bound of the wait before a failed storage write is tried again: without a wait
/// every attempt would hit the same lock within microseconds and the daemon would fail-stop
/// on a lock another program holds for a moment.
pub const MIN_STORAGE_RETRY_BACKOFF: Duration = Duration::from_millis(10);
/// The lower bound of `max_client_frame_bytes`: below it ordinary requests (`initialize`, a
/// short message) are refused as too large and the app cannot work at all.
pub const MIN_CLIENT_FRAME_BYTES: u64 = 1024;
/// The lower bound of `max_line_bytes`: ordinary protocol messages of the CLIs (a model list,
/// a tool result) are tens of KiB; a smaller limit cuts them and breaks every session.
pub const MIN_LINE_BYTES: u64 = 64 * 1024;

/// One value of a policy struct with its lower bound (docs/design.md §13).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyField {
    /// A duration and its minimum (zero when zero has a meaning, e.g. "keep nothing").
    Duration { value: Duration, min: Duration },
    /// A count or size and its minimum.
    Count { value: u64, min: u64 },
    /// A switch; every value is valid.
    Flag,
}

impl PolicyField {
    pub fn duration(value: Duration, min: Duration) -> Self {
        PolicyField::Duration { value, min }
    }

    pub fn count(value: impl TryInto<u64>, min: u64) -> Self {
        PolicyField::Count {
            value: value.try_into().unwrap_or(u64::MAX),
            min,
        }
    }

    /// Why the value is below its bound, if it is.
    fn violation(&self) -> Option<String> {
        match *self {
            PolicyField::Duration { value, min } if value < min => {
                Some(format!("must be at least {min:?} (is {value:?})"))
            }
            PolicyField::Count { value, min } if value < min => {
                Some(format!("must be at least {min} (is {value})"))
            }
            _ => None,
        }
    }
}

/// Checks every value of a policy struct against its lower bound and the struct's own rules
/// (`rules`: `(holds, message)`), reporting every violation at once as
/// `policy.<name> must be at least …`.
pub fn check_policy(
    fields: &[(&'static str, PolicyField)],
    rules: &[(bool, String)],
) -> Result<(), String> {
    let mut errors: Vec<String> = fields
        .iter()
        .filter_map(|(name, field)| field.violation().map(|v| format!("policy.{name} {v}")))
        .collect();
    errors.extend(
        rules
            .iter()
            .filter(|(holds, _)| !holds)
            .map(|(_, m)| m.clone()),
    );
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

/// Verifies that a policy struct lists every one of its keys with a lower bound, and that a
/// value below each bound is rejected with an error naming the key while the bound itself is
/// accepted (all other values at their defaults). Used by the tests of the policy structs of
/// every layer (engine, transport, daemon); returns what is wrong.
pub fn verify_policy_bounds<T>(
    fields: impl Fn(&T) -> Vec<(&'static str, PolicyField)>,
    validate: impl Fn(&T) -> Result<(), String>,
) -> Result<(), String>
where
    T: Default + Serialize + DeserializeOwned,
{
    let default = T::default();
    let keys: std::collections::BTreeSet<String> = match serde_json::to_value(&default) {
        Ok(serde_json::Value::Object(map)) => map.keys().cloned().collect(),
        other => return Err(format!("the policy does not serialize to a map: {other:?}")),
    };
    let listed = fields(&default);
    let names: std::collections::BTreeSet<String> =
        listed.iter().map(|(n, _)| (*n).to_owned()).collect();
    if names != keys {
        return Err(format!(
            "keys without a bound: {:?}; bounds without a key: {:?}",
            keys.difference(&names).collect::<Vec<_>>(),
            names.difference(&keys).collect::<Vec<_>>()
        ));
    }
    validate(&default).map_err(|e| format!("the defaults are rejected: {e}"))?;
    let with = |key: &str, value: serde_json::Value| -> Result<T, String> {
        let mut map = match serde_json::to_value(&default) {
            Ok(serde_json::Value::Object(map)) => map,
            _ => return Err("not a map".into()),
        };
        map.insert(key.to_owned(), value);
        serde_json::from_value(serde_json::Value::Object(map)).map_err(|e| format!("{key}: {e}"))
    };
    let nanos = |d: Duration| serde_json::Value::String(format!("{}ns", d.as_nanos()));
    for (name, field) in listed {
        let (below, at) = match field {
            PolicyField::Duration { min, .. } => (
                min.checked_sub(Duration::from_nanos(1)).map(nanos),
                nanos(min),
            ),
            PolicyField::Count { min, .. } => (
                min.checked_sub(1).map(serde_json::Value::from),
                serde_json::Value::from(min),
            ),
            PolicyField::Flag => continue,
        };
        if let Some(below) = below {
            match validate(&with(name, below)?) {
                Err(e) if e.contains(name) => {}
                Err(e) => {
                    return Err(format!(
                        "{name} below its bound: the error does not name it: {e}"
                    ));
                }
                Ok(()) => return Err(format!("{name} below its bound was accepted")),
            }
        }
        // The bound itself is accepted unless a rule relating it to another value (e.g.
        // `client_timeout > heartbeat_interval`) needs more; such a refusal names another key.
        if let Err(e) = validate(&with(name, at)?)
            && e.contains(&format!("policy.{name} must be at least"))
        {
            return Err(format!("{name} at its bound was rejected: {e}"));
        }
    }
    Ok(())
}

impl Policy {
    /// Every value with its lower bound (docs/design.md §13).
    pub fn fields(&self) -> Vec<(&'static str, PolicyField)> {
        use PolicyField as F;
        let d = F::duration;
        let zero = Duration::ZERO;
        vec![
            ("heartbeat_interval", d(self.heartbeat_interval, MIN_TIMER)),
            ("client_timeout", d(self.client_timeout, MIN_TIMER)),
            ("stop_grace", d(self.stop_grace, MIN_TIMER)),
            ("interrupt_grace", d(self.interrupt_grace, MIN_TIMER)),
            (
                "end_session_stop_grace",
                d(self.end_session_stop_grace, zero),
            ),
            ("idle_process_ttl", d(self.idle_process_ttl, MIN_TIMER)),
            (
                "max_running_processes",
                F::count(self.max_running_processes, 1),
            ),
            ("idempotency_ttl", d(self.idempotency_ttl, MIN_TIMER)),
            ("delta_retention", d(self.delta_retention, zero)),
            ("max_batch_events", F::count(self.max_batch_events, 1)),
            ("max_batch_bytes", F::count(self.max_batch_bytes, 1)),
            (
                "max_inline_output_bytes",
                F::count(self.max_inline_output_bytes, 1),
            ),
            (
                "max_inline_patch_bytes",
                F::count(self.max_inline_patch_bytes, 0),
            ),
            ("max_blob_bytes", F::count(self.max_blob_bytes, 1)),
            (
                "max_client_frame_bytes",
                F::count(self.max_client_frame_bytes, MIN_CLIENT_FRAME_BYTES),
            ),
            (
                "max_transport_frame_bytes",
                F::count(self.max_transport_frame_bytes, MIN_CLIENT_FRAME_BYTES),
            ),
            ("pairing_code_ttl", d(self.pairing_code_ttl, MIN_TIMER)),
            (
                "pairing_attempts_per_window",
                F::count(self.pairing_attempts_per_window, 1),
            ),
            (
                "pairing_rate_window",
                d(self.pairing_rate_window, MIN_BACKGROUND_PERIOD),
            ),
            ("device_name_chars", F::count(self.device_name_chars, 1)),
            (
                "device_platform_chars",
                F::count(self.device_platform_chars, 1),
            ),
            ("file_index_ttl", d(self.file_index_ttl, zero)),
            ("tool_timeout", d(self.tool_timeout, MIN_BACKGROUND_PERIOD)),
            (
                "clone_timeout",
                d(self.clone_timeout, MIN_BACKGROUND_PERIOD),
            ),
            (
                "operation_progress_interval",
                d(self.operation_progress_interval, zero),
            ),
            (
                "max_progress_line_bytes",
                F::count(self.max_progress_line_bytes, 1),
            ),
            (
                "background_progress_interval",
                d(self.background_progress_interval, zero),
            ),
            (
                "background_stop_confirm_timeout",
                d(self.background_stop_confirm_timeout, MIN_TIMER),
            ),
            ("stderr_tail_bytes", F::count(self.stderr_tail_bytes, 0)),
            (
                "exit_message_stderr_lines",
                F::count(self.exit_message_stderr_lines, 0),
            ),
            (
                "first_message_title_chars",
                F::count(self.first_message_title_chars, 1),
            ),
            ("harness_title_chars", F::count(self.harness_title_chars, 1)),
            (
                "queued_preview_chars",
                F::count(self.queued_preview_chars, 1),
            ),
            ("prevent_sleep_while_running", F::Flag),
            (
                "maintenance_interval",
                d(self.maintenance_interval, MIN_BACKGROUND_PERIOD),
            ),
            ("handshake_timeout", d(self.handshake_timeout, MIN_TIMER)),
            (
                "max_line_bytes",
                F::count(self.max_line_bytes, MIN_LINE_BYTES),
            ),
            (
                "kill_confirm_timeout",
                d(self.kill_confirm_timeout, MIN_TIMER),
            ),
            ("log_retention_days", F::count(self.log_retention_days, 1)),
            (
                "harness_probe_min_interval",
                d(self.harness_probe_min_interval, zero),
            ),
            (
                "harness_retry_initial_delay",
                d(self.harness_retry_initial_delay, MIN_BACKGROUND_PERIOD),
            ),
            (
                "harness_retry_max_delay",
                d(self.harness_retry_max_delay, MIN_BACKGROUND_PERIOD),
            ),
            ("sqlite_busy_timeout", d(self.sqlite_busy_timeout, zero)),
            (
                "sqlite_journal_size_limit",
                F::count(self.sqlite_journal_size_limit, 0),
            ),
            (
                "storage_retry_attempts",
                F::count(self.storage_retry_attempts, 1),
            ),
            (
                "storage_retry_initial_backoff",
                d(
                    self.storage_retry_initial_backoff,
                    MIN_STORAGE_RETRY_BACKOFF,
                ),
            ),
            (
                "storage_retry_max_backoff",
                d(self.storage_retry_max_backoff, MIN_STORAGE_RETRY_BACKOFF),
            ),
            (
                "unreferenced_blob_grace",
                d(self.unreferenced_blob_grace, MIN_TIMER),
            ),
            (
                "superseded_event_retention",
                d(self.superseded_event_retention, zero),
            ),
            (
                "native_event_retention",
                d(self.native_event_retention, zero),
            ),
            (
                "finished_operation_retention",
                d(self.finished_operation_retention, zero),
            ),
            (
                "maintenance_batch_size",
                F::count(self.maintenance_batch_size, 1),
            ),
            (
                "incremental_vacuum_pages",
                F::count(self.incremental_vacuum_pages, 1),
            ),
            (
                "max_file_search_results",
                F::count(self.max_file_search_results, 1),
            ),
            ("revocation_backlog", F::count(self.revocation_backlog, 1)),
            (
                "thread_list_default_limit",
                F::count(self.thread_list_default_limit, 1),
            ),
            (
                "thread_list_max_limit",
                F::count(self.thread_list_max_limit, 1),
            ),
            (
                "thread_read_default_turns",
                F::count(self.thread_read_default_turns, 1),
            ),
            (
                "thread_read_max_turns",
                F::count(self.thread_read_max_turns, 1),
            ),
            (
                "operation_list_limit",
                F::count(self.operation_list_limit, 1),
            ),
            (
                "snapshot_operation_limit",
                F::count(self.snapshot_operation_limit, 1),
            ),
        ]
    }

    /// Rejects values below their lower bound and inconsistent combinations, naming every
    /// offending key.
    pub fn validate(&self) -> Result<(), String> {
        let rules = [
            (
                self.client_timeout > self.heartbeat_interval,
                "policy.client_timeout must be longer than policy.heartbeat_interval".to_owned(),
            ),
            (
                self.max_transport_frame_bytes >= self.max_client_frame_bytes,
                "policy.max_transport_frame_bytes must not be smaller than policy.max_client_frame_bytes"
                    .to_owned(),
            ),
            (
                self.storage_retry_initial_backoff <= self.storage_retry_max_backoff,
                "policy.storage_retry_initial_backoff must not exceed policy.storage_retry_max_backoff"
                    .to_owned(),
            ),
            (
                self.harness_retry_initial_delay <= self.harness_retry_max_delay,
                "policy.harness_retry_initial_delay must not exceed policy.harness_retry_max_delay"
                    .to_owned(),
            ),
            (
                self.thread_list_default_limit <= self.thread_list_max_limit,
                "policy.thread_list_default_limit must not exceed policy.thread_list_max_limit"
                    .to_owned(),
            ),
            (
                self.thread_read_default_turns <= self.thread_read_max_turns,
                "policy.thread_read_default_turns must not exceed policy.thread_read_max_turns"
                    .to_owned(),
            ),
        ];
        check_policy(&self.fields(), &rules)
    }

    /// The wait before attempt `attempt + 1` of a storage write (`attempt` counts from 1).
    pub fn storage_retry_backoff(&self, attempt: u32) -> Duration {
        doubling(
            self.storage_retry_initial_backoff,
            self.storage_retry_max_backoff,
            attempt,
        )
    }

    /// The wait before an unavailable harness is probed again by itself, after `failures`
    /// consecutive probes found it unavailable (counting from 1).
    pub fn harness_retry_delay(&self, failures: u32) -> Duration {
        doubling(
            self.harness_retry_initial_delay,
            self.harness_retry_max_delay,
            failures,
        )
    }

    pub fn client_policy(&self) -> ClientPolicy {
        ClientPolicy {
            heartbeat_interval_ms: self.heartbeat_interval.as_millis() as u64,
            client_timeout_ms: self.client_timeout.as_millis() as u64,
            max_client_frame_bytes: self.max_client_frame_bytes as u64,
            max_blob_bytes: self.max_blob_bytes,
        }
    }

    pub fn supervisor_policy(&self) -> SupervisorPolicy {
        SupervisorPolicy {
            stderr_tail_bytes: self.stderr_tail_bytes,
            tool_timeout: self.tool_timeout,
            kill_confirm_timeout: self.kill_confirm_timeout,
            prevent_sleep: self.prevent_sleep_while_running,
        }
    }

    pub fn adapter_policy(&self) -> AdapterPolicy {
        AdapterPolicy {
            stop_grace: self.stop_grace,
            max_line_bytes: self.max_line_bytes,
            handshake_timeout: self.handshake_timeout,
            first_message_title_chars: self.first_message_title_chars,
            harness_title_chars: self.harness_title_chars,
        }
    }

    pub fn batch_limits(&self) -> BatchLimits {
        BatchLimits {
            max_events: self.max_batch_events,
            max_bytes: self.max_batch_bytes,
        }
    }

    pub(crate) fn db_options(&self) -> crate::db::DbOptions {
        crate::db::DbOptions {
            busy_timeout: self.sqlite_busy_timeout,
            journal_size_limit: self.sqlite_journal_size_limit,
        }
    }
}

/// `initial` doubled for every step after the first (`step` counts from 1), at most `max`.
fn doubling(initial: Duration, max: Duration, step: u32) -> Duration {
    let doublings = step.saturating_sub(1).min(31);
    initial.saturating_mul(1u32 << doublings).min(max)
}

/// Thresholds of the few heuristics the engine uses (`[heuristics]`; see design.md §14).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HeuristicsConfig {
    /// H1: number of fuzzy-ranked results returned by `fs/search`.
    pub file_search_max_results: usize,
}

impl Default for HeuristicsConfig {
    fn default() -> Self {
        Self {
            file_search_max_results: 50,
        }
    }
}

/// Everything the engine needs to know about its environment.
#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// Root of the daemon's data (`%LOCALAPPDATA%\agent-app-server`).
    pub data_dir: PathBuf,
    pub server_name: String,
    pub hostname: String,
    /// Folders the API may browse and create projects in.
    pub project_roots: Vec<PathBuf>,
    pub policy: Policy,
    pub heuristics: HeuristicsConfig,
    /// `git` executable; `None` disables diffs, worktrees and clone.
    pub git: Option<PathBuf>,
}

impl EngineConfig {
    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("aas.db")
    }
    pub fn blobs_dir(&self) -> PathBuf {
        self.data_dir.join("blobs")
    }
    pub fn worktrees_dir(&self) -> PathBuf {
        self.data_dir.join("worktrees")
    }
    pub fn tmp_dir(&self) -> PathBuf {
        self.data_dir.join("tmp")
    }
    pub fn supervisor_dir(&self) -> PathBuf {
        self.data_dir.join("supervisor")
    }
    pub fn adapter_state_dir(&self, harness_id: &str) -> PathBuf {
        self.data_dir.join("adapters").join(harness_id)
    }

    pub fn ensure_dirs(&self) -> std::io::Result<()> {
        for dir in [
            self.data_dir.clone(),
            self.blobs_dir(),
            self.worktrees_dir(),
            self.tmp_dir(),
            self.supervisor_dir(),
        ] {
            std::fs::create_dir_all(&dir)?;
        }
        Ok(())
    }
}

/// Canonicalizes project roots (dropping the ones that do not exist).
pub fn canonical_roots(roots: &[PathBuf]) -> Vec<PathBuf> {
    roots
        .iter()
        .filter_map(|r| match dunce::canonicalize(r) {
            Ok(p) => Some(p),
            Err(e) => {
                tracing::warn!(root = %r.display(), error = %e, "project root is not accessible; ignored");
                None
            }
        })
        .collect()
}

/// Whether `path` is `root` or inside it (component-wise; case-insensitive on Windows).
pub fn path_within(path: &Path, root: &Path) -> bool {
    let mut p = path.components();
    for rc in root.components() {
        match p.next() {
            Some(pc) if component_eq(pc.as_os_str(), rc.as_os_str()) => {}
            _ => return false,
        }
    }
    true
}

fn component_eq(a: &std::ffi::OsStr, b: &std::ffi::OsStr) -> bool {
    if cfg!(windows) {
        a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase()
    } else {
        a == b
    }
}

/// Key used to detect the same folder registered twice (case-folded on Windows).
pub fn path_key(path: &Path) -> String {
    let s = path.to_string_lossy().replace('/', "\\");
    if cfg!(windows) {
        s.to_lowercase()
    } else {
        path.to_string_lossy().into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_policy_is_valid_and_round_trips_through_toml_like_json() {
        let p = Policy::default();
        p.validate().unwrap();
        let json = serde_json::to_value(&p).unwrap();
        assert_eq!(json["heartbeat_interval"], "15s");
        let back: Policy = serde_json::from_value(json).unwrap();
        assert_eq!(back, p);
    }

    #[test]
    fn the_adapter_policy_of_the_defaults_is_the_adapters_default() {
        // design.md §13: `AdapterPolicy::default()` holds the same values as the table.
        assert_eq!(
            format!("{:?}", Policy::default().adapter_policy()),
            format!("{:?}", AdapterPolicy::default())
        );
    }

    #[test]
    fn inconsistent_policy_is_rejected() {
        let p = Policy {
            client_timeout: Duration::from_secs(10),
            ..Policy::default()
        };
        assert!(p.validate().is_err());
        let invalid = [
            Policy {
                storage_retry_attempts: 0,
                ..Policy::default()
            },
            Policy {
                storage_retry_initial_backoff: Duration::from_secs(10),
                ..Policy::default()
            },
            Policy {
                maintenance_batch_size: 0,
                ..Policy::default()
            },
            Policy {
                revocation_backlog: 0,
                ..Policy::default()
            },
            Policy {
                thread_list_default_limit: 600,
                ..Policy::default()
            },
            Policy {
                thread_read_default_turns: 300,
                ..Policy::default()
            },
        ];
        for p in invalid {
            assert!(p.validate().is_err(), "{p:?} was accepted");
        }
    }

    #[test]
    fn every_policy_value_has_a_lower_bound_that_is_enforced() {
        verify_policy_bounds(Policy::fields, Policy::validate).unwrap();
        // The values that used to take the daemon down.
        for (key, value) in [
            ("heartbeat_interval", "0s"),
            ("maintenance_interval", "0s"),
            ("stop_grace", "0s"),
            ("handshake_timeout", "0s"),
            ("pairing_rate_window", "0s"),
        ] {
            let mut json = serde_json::to_value(Policy::default()).unwrap();
            json[key] = serde_json::Value::from(value);
            let p: Policy = serde_json::from_value(json).unwrap();
            let err = p.validate().unwrap_err();
            assert!(
                err.contains(&format!("policy.{key} must be at least")),
                "{err}"
            );
        }
        // Every violation is reported, not only the first.
        let p = Policy {
            heartbeat_interval: Duration::ZERO,
            maintenance_interval: Duration::ZERO,
            ..Policy::default()
        };
        let err = p.validate().unwrap_err();
        assert!(err.contains("heartbeat_interval") && err.contains("maintenance_interval"));
        // Zero is meaningful where the bound is zero ("keep nothing", "every line").
        let p = Policy {
            delta_retention: Duration::ZERO,
            operation_progress_interval: Duration::ZERO,
            end_session_stop_grace: Duration::ZERO,
            harness_probe_min_interval: Duration::ZERO,
            ..Policy::default()
        };
        p.validate().unwrap();
    }

    #[test]
    fn the_old_name_of_the_pairing_limit_is_still_accepted() {
        let p: Policy =
            serde_json::from_value(serde_json::json!({"pairing_attempts_per_minute": 3})).unwrap();
        assert_eq!(p.pairing_attempts_per_window, 3);
        let json = serde_json::to_value(&p).unwrap();
        assert_eq!(json["pairing_attempts_per_window"], 3);
        assert!(json.get("pairing_attempts_per_minute").is_none());
    }

    #[test]
    fn harness_retries_double_up_to_the_maximum() {
        let p = Policy {
            harness_retry_initial_delay: Duration::from_secs(30),
            harness_retry_max_delay: Duration::from_secs(300),
            ..Policy::default()
        };
        let waits: Vec<u64> = (1..=6)
            .map(|f| p.harness_retry_delay(f).as_secs())
            .collect();
        assert_eq!(waits, vec![30, 60, 120, 240, 300, 300]);
        assert_eq!(p.harness_retry_delay(u32::MAX), Duration::from_secs(300));
        let backwards = Policy {
            harness_retry_initial_delay: Duration::from_secs(600),
            ..p
        };
        assert!(backwards.validate().is_err());
    }

    #[test]
    fn storage_retry_backoff_doubles_up_to_the_maximum() {
        let p = Policy {
            storage_retry_initial_backoff: Duration::from_millis(100),
            storage_retry_max_backoff: Duration::from_millis(500),
            ..Policy::default()
        };
        let waits: Vec<u128> = (1..=5)
            .map(|a| p.storage_retry_backoff(a).as_millis())
            .collect();
        assert_eq!(waits, vec![100, 200, 400, 500, 500]);
        assert_eq!(
            p.storage_retry_backoff(1000),
            Duration::from_millis(500),
            "no overflow"
        );
    }

    #[test]
    fn containment_is_component_wise() {
        let root = Path::new("C:\\Users\\me\\Documents");
        assert!(path_within(
            Path::new("C:\\Users\\me\\Documents\\proj"),
            root
        ));
        assert!(path_within(root, root));
        assert!(!path_within(
            Path::new("C:\\Users\\me\\DocumentsX\\proj"),
            root
        ));
        assert!(!path_within(Path::new("C:\\Users\\me"), root));
        if cfg!(windows) {
            assert!(path_within(Path::new("c:\\users\\ME\\documents\\p"), root));
        }
    }
}
