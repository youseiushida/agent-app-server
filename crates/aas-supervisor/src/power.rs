//! Keeps the machine awake while leases are held: one per running turn, and one for the
//! daemon's whole life when it is configured to keep the PC awake always.
//!
//! `SetThreadExecutionState` is per-thread, so a dedicated thread owns the state and applies
//! transitions: the first lease sets `ES_CONTINUOUS | ES_SYSTEM_REQUIRED`, dropping the last
//! lease restores `ES_CONTINUOUS`.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;

enum PowerCmd {
    Acquire,
    Release,
    /// Applies the current request again and answers with the execution state the OS held for
    /// the power thread before it (the thread's own request, as the OS reports it).
    #[cfg(test)]
    Observe(mpsc::Sender<Option<u32>>),
}

/// Counts active leases and inhibits system sleep while at least one exists.
#[derive(Clone)]
pub struct PowerGuard {
    tx: Option<mpsc::Sender<PowerCmd>>,
    active: Arc<AtomicUsize>,
}

/// Held while a turn runs; dropping it releases the inhibition.
pub struct PowerLease {
    tx: Option<mpsc::Sender<PowerCmd>>,
    active: Arc<AtomicUsize>,
}

/// Runs the power thread: applies every transition of the lease count.
fn power_thread(rx: mpsc::Receiver<PowerCmd>) {
    let mut count = 0usize;
    while let Ok(cmd) = rx.recv() {
        match cmd {
            PowerCmd::Acquire => {
                count += 1;
                if count == 1 && imp::inhibit_sleep(true).is_some() {
                    tracing::info!(leases = count, "system sleep inhibited");
                }
            }
            PowerCmd::Release => {
                count = count.saturating_sub(1);
                if count == 0 && imp::inhibit_sleep(false).is_some() {
                    tracing::info!("system sleep inhibition released");
                }
            }
            #[cfg(test)]
            PowerCmd::Observe(reply) => {
                // Re-applying the same flags leaves the request as it is; the call reports the
                // state it replaced.
                let _ = reply.send(imp::inhibit_sleep(count > 0));
            }
        }
    }
    // Every sender is gone, and every lease sent its release before going: `count` is 0 and the
    // request was already released. The thread's request ends with the thread in any case.
}

impl PowerGuard {
    /// `enabled = false` makes leases pure counters (for tests and for users who opt out).
    pub fn new(enabled: bool) -> Self {
        let active = Arc::new(AtomicUsize::new(0));
        if !enabled {
            return Self { tx: None, active };
        }
        let (tx, rx) = mpsc::channel::<PowerCmd>();
        let spawned = std::thread::Builder::new()
            .name("aas-power".into())
            .spawn(move || power_thread(rx));
        match spawned {
            Ok(_) => Self {
                tx: Some(tx),
                active,
            },
            Err(e) => {
                tracing::error!(error = %e, "could not start the power thread; sleep will not be inhibited");
                Self { tx: None, active }
            }
        }
    }

    pub fn acquire(&self) -> PowerLease {
        self.active.fetch_add(1, Ordering::SeqCst);
        if let Some(tx) = &self.tx {
            send(tx, PowerCmd::Acquire);
        }
        PowerLease {
            tx: self.tx.clone(),
            active: self.active.clone(),
        }
    }

    /// Number of live leases.
    pub fn active(&self) -> usize {
        self.active.load(Ordering::SeqCst)
    }

    /// The execution state the OS holds for the power thread, read back through the OS after
    /// every command sent before this call has been applied. `None` when leases are pure
    /// counters or the OS call failed.
    #[cfg(test)]
    fn observe(&self) -> Option<u32> {
        let tx = self.tx.as_ref()?;
        let (reply, answer) = mpsc::channel();
        send(tx, PowerCmd::Observe(reply));
        answer.recv().ok().flatten()
    }
}

/// Hands a command to the power thread. The thread runs until every sender is gone, so a
/// failed send means it died (a panic): sleep is no longer managed, which the log must show.
fn send(tx: &mpsc::Sender<PowerCmd>, cmd: PowerCmd) {
    if tx.send(cmd).is_err() {
        tracing::error!("the power thread is gone; system sleep is no longer managed");
    }
}

impl Drop for PowerLease {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::SeqCst);
        if let Some(tx) = &self.tx {
            send(tx, PowerCmd::Release);
        }
    }
}

#[cfg(windows)]
mod imp {
    use windows::Win32::System::Power::{
        ES_CONTINUOUS, ES_SYSTEM_REQUIRED, SetThreadExecutionState,
    };

    /// Sets (`on`) or clears the calling thread's continuous system-required request. Returns
    /// the thread's previous execution state, or `None` when the call failed (logged).
    pub fn inhibit_sleep(on: bool) -> Option<u32> {
        let flags = if on {
            ES_CONTINUOUS | ES_SYSTEM_REQUIRED
        } else {
            ES_CONTINUOUS
        };
        // SAFETY: SetThreadExecutionState only reads the flags value.
        let previous = unsafe { SetThreadExecutionState(flags) };
        if previous.0 == 0 {
            tracing::warn!(
                on,
                error = %std::io::Error::last_os_error(),
                "SetThreadExecutionState failed"
            );
            return None;
        }
        Some(previous.0)
    }
}

#[cfg(not(windows))]
mod imp {
    /// Sleep inhibition is implemented for Windows only (the daemon's platform); elsewhere the
    /// leases are counted and nothing is requested.
    pub fn inhibit_sleep(_on: bool) -> Option<u32> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leases_are_counted() {
        let guard = PowerGuard::new(true);
        assert_eq!(guard.active(), 0);
        let a = guard.acquire();
        let b = guard.acquire();
        assert_eq!(guard.active(), 2);
        drop(a);
        assert_eq!(guard.active(), 1);
        drop(b);
        assert_eq!(guard.active(), 0);
    }

    #[test]
    fn disabled_guards_request_nothing() {
        let guard = PowerGuard::new(false);
        let lease = guard.acquire();
        assert_eq!(guard.active(), 1);
        assert_eq!(guard.observe(), None, "no power thread, no OS request");
        drop(lease);
        assert_eq!(guard.active(), 0);
    }

    /// Checks the leases against what the OS reports, not against the guard's own counter.
    ///
    /// * While a lease is held, the system-wide execution state
    ///   (`CallNtPowerInformation(SystemExecutionState)`) has `ES_SYSTEM_REQUIRED`: the system
    ///   is kept awake, whatever other processes request.
    /// * The daemon's own request is the power thread's execution state, which the OS returns
    ///   from `SetThreadExecutionState`. Other processes may hold requests of their own, so the
    ///   release is asserted on that state only: it keeps `ES_CONTINUOUS` and loses
    ///   `ES_SYSTEM_REQUIRED` once the last lease is gone.
    #[cfg(windows)]
    #[test]
    fn the_os_sees_the_request_while_a_lease_is_held_and_not_after_the_last_release() {
        use windows::Win32::Foundation::STATUS_SUCCESS;
        use windows::Win32::System::Power::{
            CallNtPowerInformation, ES_CONTINUOUS, ES_SYSTEM_REQUIRED, SystemExecutionState,
        };

        fn system_execution_state() -> u32 {
            let mut state = 0u32;
            // SAFETY: the output buffer is a live u32 of the size passed; no input buffer.
            let status = unsafe {
                CallNtPowerInformation(
                    SystemExecutionState,
                    None,
                    0,
                    Some((&mut state as *mut u32).cast()),
                    std::mem::size_of::<u32>() as u32,
                )
            };
            assert_eq!(status, STATUS_SUCCESS, "CallNtPowerInformation failed");
            state
        }

        let requested = ES_CONTINUOUS.0 | ES_SYSTEM_REQUIRED.0;
        let guard = PowerGuard::new(true);

        let first = guard.acquire();
        let ours = guard.observe().expect("the OS reports the thread's state");
        assert_eq!(ours & requested, requested, "our request: {ours:#x}");
        let system = system_execution_state();
        assert_ne!(
            system & ES_SYSTEM_REQUIRED.0,
            0,
            "the system is kept awake while a lease is held: {system:#x}"
        );

        // A second lease changes nothing; dropping one of two keeps the request.
        let second = guard.acquire();
        drop(first);
        let ours = guard.observe().expect("the OS reports the thread's state");
        assert_eq!(ours & requested, requested, "one lease is left: {ours:#x}");
        assert_ne!(system_execution_state() & ES_SYSTEM_REQUIRED.0, 0);

        drop(second);
        let ours = guard.observe().expect("the OS reports the thread's state");
        assert_eq!(
            ours & ES_SYSTEM_REQUIRED.0,
            0,
            "our request is released: {ours:#x}"
        );
        assert_ne!(
            ours & ES_CONTINUOUS.0,
            0,
            "the thread keeps a continuous state: {ours:#x}"
        );
    }
}
