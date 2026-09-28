//! Test helpers shared by the process-tree, end-to-end and chaos tests.
//!
//! * `aas-dummy-agent` (binary): the fake agent as a real process, plus process-tree modes.
//! * `aas-supervisor-host` (binary): a stand-in daemon that owns a supervised tree, so a test
//!   can kill it and check that `KILL_ON_JOB_CLOSE` takes the tree down.
//! * [`proc`]: process identities (PID + creation time) recorded by the dummy agent, liveness
//!   checks and a cleanup guard.
//! * [`chaos::ChaosProxy`]: a TCP proxy that drops, delays and blackholes connections.
//! * [`client::ReliableClient`]: a reference implementation of the client obligations of
//!   docs/protocol.md §7 (cursors, outbox resend, watchdog, reconnect).

pub mod chaos;
pub mod client;
pub mod proc;
