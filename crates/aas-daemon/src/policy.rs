//! Policy values of the daemon process itself: the admin client, `doctor`, the watchdog and
//! its liveness contract, and the end-session shutdown. They are written under `[policy]` in
//! config.toml next to the engine's and the transport's values (see [`crate::config`]);
//! defaults and their rationale are listed in docs/design.md §13.

use std::time::Duration;

use aas_core::config::{MIN_BACKGROUND_PERIOD, MIN_TIMER, PolicyField, check_policy};
use serde::{Deserialize, Serialize};

/// The shortest repetition interval Task Scheduler accepts for a trigger (one minute) and the
/// longest (31 days): the bounds of `autostart_keepalive_interval`.
pub const TASK_SCHEDULER_MIN_REPETITION: Duration = Duration::from_secs(60);
pub const TASK_SCHEDULER_MAX_REPETITION: Duration = Duration::from_secs(31 * 24 * 3600);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DaemonPolicy {
    /// Upper bound for the CLI (and the watchdog) to connect to the admin listener. The daemon
    /// is on the same machine: a connection is immediate or refused, so a few seconds only
    /// cover a machine under heavy load.
    #[serde(with = "humantime_serde")]
    pub admin_connect_timeout: Duration,
    /// Upper bound of one admin request (`pair`, `status`, `stop`, …) after connecting. Each
    /// of these is a database read or write; thirty seconds cover a busy disk without letting
    /// the CLI hang forever on a daemon that stopped answering. `harness refresh` waits for
    /// probes of agent CLIs instead and has its own bound (`harness_refresh_timeout`).
    #[serde(with = "humantime_serde")]
    pub admin_request_timeout: Duration,
    /// Upper bound of `agent-app-server harness refresh` after connecting. The daemon answers
    /// once the probes it started have finished: a probe runs several steps that are each
    /// bounded by the engine's `handshake_timeout` (Codex: `--version`, the app-server
    /// handshake, `model/list`; pi also reads its state), and a refresh first waits for a probe
    /// that is already running. Ten minutes cover two such probes of three or four steps with
    /// the default `handshake_timeout` (60 seconds) and still end a wait for a daemon that
    /// stopped answering. When it passes, the probes go on in the daemon and their results
    /// reach the app (`harness/updated`). Must be longer than `handshake_timeout`: a shorter
    /// bound fails as soon as one step of a probe is slow, the case the command exists for.
    #[serde(with = "humantime_serde")]
    pub harness_refresh_timeout: Duration,
    /// Upper bound of `<cli> --version` in `doctor`. Node-based CLIs can take several seconds
    /// on their first start (cold cache, antivirus scan).
    #[serde(with = "humantime_serde")]
    pub doctor_version_timeout: Duration,
    /// First restart delay after the daemon exits unexpectedly.
    #[serde(with = "humantime_serde")]
    pub watchdog_restart_delay_min: Duration,
    /// Upper bound of the doubling restart delay while the daemon keeps failing: a daemon that
    /// cannot start does not spin, and one that recovers is back within a minute.
    #[serde(with = "humantime_serde")]
    pub watchdog_restart_delay_max: Duration,
    /// A run lasting at least this long resets the restart delay (it was not a crash loop).
    #[serde(with = "humantime_serde")]
    pub watchdog_stable_run: Duration,
    /// How long the watchdog waits for the daemon's ready line (both listeners bound, engine
    /// started, recovery done). Startup normally takes a second or two; a migration of a large
    /// database takes longer. A daemon that has not reported ready by then is treated as hung
    /// and restarted.
    #[serde(with = "humantime_serde")]
    pub watchdog_ready_timeout: Duration,
    /// Period of the watchdog's liveness checks (`GET /v1/liveness` on the admin listener)
    /// once the daemon is ready. Hangs are rare and a check is cheap; thirty seconds find one
    /// within minutes without adding noticeable load.
    #[serde(with = "humantime_serde")]
    pub liveness_interval: Duration,
    /// Upper bound of one liveness check (connect, request, answer). Longer than
    /// `liveness_deadline`, so that a daemon which is slow but alive answers 503 in time
    /// instead of being cut off.
    #[serde(with = "humantime_serde")]
    pub liveness_timeout: Duration,
    /// Consecutive failed liveness checks after which the watchdog kills the daemon's process
    /// tree and restarts it. Three tolerate a single slow moment (resume from sleep, a disk
    /// spinning up) while a real hang is resolved within about two minutes.
    pub liveness_failures: u32,
    /// Time budget of the shutdown when Windows ends the session (sign-out, shutdown, reboot)
    /// or the console window is closed. Windows gives a top-level window about five seconds to
    /// return from `WM_ENDSESSION` (and a console process as long after `CTRL_CLOSE_EVENT`)
    /// before it may terminate the process; four seconds leave one second for the process to
    /// exit. Whatever has not stopped by then goes with the Job Objects. The engine's
    /// `end_session_stop_grace` (how long the agents get) must be shorter.
    #[serde(with = "humantime_serde")]
    pub end_session_deadline: Duration,
    /// Period of the keep-alive task `autostart install` registers next to the logon task
    /// (design.md §18.3): it starts the watchdog again when it is not running although it was
    /// not stopped on purpose (a crash of the watchdog itself; Task Scheduler does not restart
    /// a task whose program exits with a non-zero code). Five minutes bound the time without a
    /// daemon after such a crash; a start that finds the watchdog running ends at once.
    /// Written into the task at `autostart install` (run it again after a change).
    #[serde(with = "humantime_serde")]
    pub autostart_keepalive_interval: Duration,
}

impl Default for DaemonPolicy {
    fn default() -> Self {
        Self {
            admin_connect_timeout: Duration::from_secs(5),
            admin_request_timeout: Duration::from_secs(30),
            harness_refresh_timeout: Duration::from_secs(10 * 60),
            doctor_version_timeout: Duration::from_secs(30),
            watchdog_restart_delay_min: Duration::from_secs(2),
            watchdog_restart_delay_max: Duration::from_secs(60),
            watchdog_stable_run: Duration::from_secs(10 * 60),
            watchdog_ready_timeout: Duration::from_secs(120),
            liveness_interval: Duration::from_secs(30),
            liveness_timeout: Duration::from_secs(10),
            liveness_failures: 3,
            end_session_deadline: Duration::from_secs(4),
            autostart_keepalive_interval: Duration::from_secs(5 * 60),
        }
    }
}

impl DaemonPolicy {
    /// Every value with its lower bound (docs/design.md §13).
    pub fn fields(&self) -> Vec<(&'static str, PolicyField)> {
        let d = PolicyField::duration;
        vec![
            (
                "admin_connect_timeout",
                d(self.admin_connect_timeout, MIN_TIMER),
            ),
            (
                "admin_request_timeout",
                d(self.admin_request_timeout, MIN_TIMER),
            ),
            (
                "harness_refresh_timeout",
                d(self.harness_refresh_timeout, MIN_TIMER),
            ),
            (
                "doctor_version_timeout",
                d(self.doctor_version_timeout, MIN_BACKGROUND_PERIOD),
            ),
            (
                "watchdog_restart_delay_min",
                d(self.watchdog_restart_delay_min, MIN_TIMER),
            ),
            (
                "watchdog_restart_delay_max",
                d(self.watchdog_restart_delay_max, MIN_TIMER),
            ),
            (
                "watchdog_stable_run",
                d(self.watchdog_stable_run, MIN_BACKGROUND_PERIOD),
            ),
            (
                "watchdog_ready_timeout",
                d(self.watchdog_ready_timeout, MIN_BACKGROUND_PERIOD),
            ),
            ("liveness_interval", d(self.liveness_interval, MIN_TIMER)),
            ("liveness_timeout", d(self.liveness_timeout, MIN_TIMER)),
            (
                "liveness_failures",
                PolicyField::count(self.liveness_failures, 1),
            ),
            (
                "end_session_deadline",
                d(self.end_session_deadline, MIN_TIMER),
            ),
            (
                "autostart_keepalive_interval",
                d(
                    self.autostart_keepalive_interval,
                    TASK_SCHEDULER_MIN_REPETITION,
                ),
            ),
        ]
    }

    /// Rejects values below their lower bound and values the daemon and the watchdog cannot
    /// work with, naming every offending key. `liveness_deadline` is the transport's deadline
    /// of the liveness round trip, which a check must outlast; `handshake_timeout` is the
    /// engine's bound of one step of a probe, which `harness refresh` must outlast.
    pub fn validate(
        &self,
        liveness_deadline: Duration,
        handshake_timeout: Duration,
    ) -> Result<(), String> {
        check_policy(
            &self.fields(),
            &[
                (
                    self.harness_refresh_timeout > handshake_timeout,
                    "policy.harness_refresh_timeout must be longer than policy.handshake_timeout"
                        .to_owned(),
                ),
                (
                    self.watchdog_restart_delay_min <= self.watchdog_restart_delay_max,
                    "policy.watchdog_restart_delay_min must not exceed policy.watchdog_restart_delay_max"
                        .to_owned(),
                ),
                (
                    self.liveness_timeout > liveness_deadline,
                    "policy.liveness_timeout must be longer than policy.liveness_deadline"
                        .to_owned(),
                ),
                (
                    self.autostart_keepalive_interval <= TASK_SCHEDULER_MAX_REPETITION,
                    format!(
                        "policy.autostart_keepalive_interval must not exceed {:?} (Task Scheduler's longest repetition)",
                        TASK_SCHEDULER_MAX_REPETITION
                    ),
                ),
            ],
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid_and_inconsistent_values_are_rejected() {
        let deadline = aas_server::ServerPolicy::default().liveness_deadline;
        let handshake = aas_core::Policy::default().handshake_timeout;
        DaemonPolicy::default()
            .validate(deadline, handshake)
            .unwrap();
        let bad = [
            DaemonPolicy {
                harness_refresh_timeout: handshake,
                ..DaemonPolicy::default()
            },
            DaemonPolicy {
                liveness_failures: 0,
                ..DaemonPolicy::default()
            },
            DaemonPolicy {
                liveness_timeout: deadline,
                ..DaemonPolicy::default()
            },
            DaemonPolicy {
                watchdog_restart_delay_min: Duration::from_secs(120),
                ..DaemonPolicy::default()
            },
            DaemonPolicy {
                end_session_deadline: Duration::ZERO,
                ..DaemonPolicy::default()
            },
        ];
        for p in bad {
            assert!(p.validate(deadline, handshake).is_err(), "{p:?}");
        }
        // `harness refresh` waits for probes, whose steps are each bounded by the engine's
        // `handshake_timeout`: its bound follows that value, not the admin request timeout.
        let err = DaemonPolicy {
            harness_refresh_timeout: Duration::from_secs(90),
            ..DaemonPolicy::default()
        }
        .validate(deadline, Duration::from_secs(120))
        .unwrap_err();
        assert!(err.contains("policy.harness_refresh_timeout"), "{err}");
        assert!(
            DaemonPolicy::default().harness_refresh_timeout
                > DaemonPolicy::default().admin_request_timeout
        );
    }

    #[test]
    fn every_daemon_value_has_a_lower_bound_that_is_enforced() {
        let deadline = aas_server::ServerPolicy::default().liveness_deadline;
        let handshake = aas_core::Policy::default().handshake_timeout;
        aas_core::config::verify_policy_bounds(DaemonPolicy::fields, |p: &DaemonPolicy| {
            p.validate(deadline, handshake)
        })
        .unwrap();
        let p = DaemonPolicy {
            autostart_keepalive_interval: TASK_SCHEDULER_MAX_REPETITION + Duration::from_secs(1),
            ..DaemonPolicy::default()
        };
        assert!(
            p.validate(deadline, handshake)
                .unwrap_err()
                .contains("autostart_keepalive_interval")
        );
    }
}
