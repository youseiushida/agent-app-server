//! Why and how urgently the daemon stops.
//!
//! Sources, followed for the daemon's whole life so that a later one can still escalate:
//! * `agent-app-server stop [--drain]` (admin API);
//! * Ctrl+C / Ctrl+Break in a console;
//! * the end of the Windows session: `WM_ENDSESSION` on the hidden window
//!   ([`crate::endsession`]), `CTRL_CLOSE_EVENT` / `CTRL_LOGOFF_EVENT` / `CTRL_SHUTDOWN_EVENT`
//!   for console runs, and the watchdog's `end-session` control line (it got the message
//!   itself and passes it on, so the daemon does not depend on the order in which Windows
//!   notifies processes).

use std::sync::Arc;
use std::time::Duration;

use aas_server::StopRequest;
use parking_lot::Mutex;
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;

use crate::endsession::EndSession;

/// How the daemon was asked to stop. The level only escalates: drain → now → end of session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopLevel {
    /// Stop accepting turns and wait for the running ones to finish.
    Drain,
    /// Stop now (running turns are interrupted).
    Now,
    /// Windows is ending the session: stop now and be done by `deadline`.
    EndSession { deadline: Instant },
}

pub(crate) fn escalate(current: Option<StopLevel>, requested: StopLevel) -> Option<StopLevel> {
    use StopLevel::*;
    Some(match (current, requested) {
        (Some(EndSession { deadline: a }), EndSession { deadline: b }) => {
            EndSession { deadline: a.min(b) }
        }
        (Some(e @ EndSession { .. }), _) | (_, e @ EndSession { .. }) => e,
        (Some(Now), _) | (_, Now) => Now,
        _ => Drain,
    })
}

/// The followed stop level plus the end-session requests whose senders wait for the shutdown
/// to finish.
pub struct StopSignals {
    level: watch::Receiver<Option<StopLevel>>,
    pending: Arc<Mutex<Vec<EndSession>>>,
}

impl StopSignals {
    pub fn level(&self) -> watch::Receiver<Option<StopLevel>> {
        self.level.clone()
    }

    /// Releases every waiting end-session request (call once the shutdown is done).
    pub fn acknowledge_end_session(&self) {
        for request in self.pending.lock().drain(..) {
            request.ack();
        }
    }
}

/// Where end-session requests come from besides the admin API.
pub struct EndSessionSources {
    /// Requests from the hidden window and the watchdog's control line.
    pub requests: mpsc::UnboundedReceiver<EndSession>,
    /// Budget of a console close / logoff / shutdown event (`policy.end_session_deadline`).
    pub budget: Duration,
    /// Listen for console control events (`SetConsoleCtrlHandler`).
    pub console: bool,
}

/// Follows stop requests of the admin API and Ctrl+C (no end-session sources).
pub fn follow_stop_requests(
    requests: watch::Receiver<Option<StopRequest>>,
) -> watch::Receiver<Option<StopLevel>> {
    let (_tx, rx) = mpsc::unbounded_channel();
    follow_signals(
        requests,
        EndSessionSources {
            requests: rx,
            budget: Duration::ZERO,
            console: false,
        },
    )
    .level
}

/// Console control events that end the session (close, logoff, shutdown), as one stream.
#[cfg(windows)]
fn console_end_events() -> std::io::Result<mpsc::UnboundedReceiver<&'static str>> {
    use tokio::signal::windows::{ctrl_close, ctrl_logoff, ctrl_shutdown};
    let (tx, rx) = mpsc::unbounded_channel();
    let mut close = ctrl_close()?;
    let mut logoff = ctrl_logoff()?;
    let mut shutdown = ctrl_shutdown()?;
    tokio::spawn(async move {
        loop {
            let event = tokio::select! {
                Some(()) = close.recv() => "CTRL_CLOSE_EVENT",
                Some(()) = logoff.recv() => "CTRL_LOGOFF_EVENT",
                Some(()) = shutdown.recv() => "CTRL_SHUTDOWN_EVENT",
                else => return,
            };
            if tx.send(event).is_err() {
                return;
            }
        }
    });
    Ok(rx)
}

#[cfg(not(windows))]
fn console_end_events() -> std::io::Result<mpsc::UnboundedReceiver<&'static str>> {
    let (_tx, rx) = mpsc::unbounded_channel();
    Ok(rx)
}

/// Ctrl+C and (on Windows) Ctrl+Break, registered once so that no event falls between two
/// waits.
struct Interrupts {
    #[cfg(windows)]
    ctrl_c: tokio::signal::windows::CtrlC,
    #[cfg(windows)]
    ctrl_break: tokio::signal::windows::CtrlBreak,
}

impl Interrupts {
    fn new() -> std::io::Result<Self> {
        #[cfg(windows)]
        {
            Ok(Self {
                ctrl_c: tokio::signal::windows::ctrl_c()?,
                ctrl_break: tokio::signal::windows::ctrl_break()?,
            })
        }
        #[cfg(not(windows))]
        {
            Ok(Self {})
        }
    }

    /// The next interrupt; `None` once no more can arrive.
    async fn next(&mut self) -> Option<()> {
        #[cfg(windows)]
        {
            tokio::select! {
                Some(()) = self.ctrl_c.recv() => Some(()),
                Some(()) = self.ctrl_break.recv() => Some(()),
                else => None,
            }
        }
        #[cfg(not(windows))]
        {
            tokio::signal::ctrl_c().await.ok()
        }
    }
}

/// Follows every stop source for the daemon's whole life.
pub fn follow_signals(
    mut requests: watch::Receiver<Option<StopRequest>>,
    sources: EndSessionSources,
) -> StopSignals {
    let (level_tx, level_rx) = watch::channel(None);
    let pending: Arc<Mutex<Vec<EndSession>>> = Arc::default();
    let held = pending.clone();
    let EndSessionSources {
        requests: mut end_requests,
        budget,
        console,
    } = sources;
    let mut console_events = if console {
        match console_end_events() {
            Ok(rx) => Some(rx),
            Err(e) => {
                tracing::warn!(error = %e, "cannot listen for console close / logoff / shutdown events");
                None
            }
        }
    } else {
        None
    };
    let mut interrupts = match Interrupts::new() {
        Ok(i) => Some(i),
        Err(e) => {
            tracing::warn!(error = %e, "cannot listen for Ctrl+C / Ctrl+Break");
            None
        }
    };
    tokio::spawn(async move {
        let mut end_requests_open = true;
        loop {
            let requested = tokio::select! {
                interrupted = async { interrupts.as_mut().expect("guarded").next().await }, if interrupts.is_some() => match interrupted {
                    Some(()) => {
                        tracing::info!("interrupted (Ctrl+C / Ctrl+Break): stopping");
                        Some(StopLevel::Now)
                    }
                    None => {
                        interrupts = None;
                        None
                    }
                },
                changed = requests.changed() => {
                    if changed.is_err() {
                        return;
                    }
                    let request = *requests.borrow_and_update();
                    request.map(|r| if r.drain { StopLevel::Drain } else { StopLevel::Now })
                }
                request = end_requests.recv(), if end_requests_open => match request {
                    Some(request) => {
                        let deadline = Instant::now() + request.remaining();
                        held.lock().push(request);
                        Some(StopLevel::EndSession { deadline })
                    }
                    None => {
                        end_requests_open = false;
                        None
                    }
                },
                event = async { console_events.as_mut().expect("guarded").recv().await }, if console_events.is_some() => match event {
                    Some(event) => {
                        tracing::warn!(event, "the console is closing or the session is ending; shutting down");
                        Some(StopLevel::EndSession { deadline: Instant::now() + budget })
                    }
                    None => {
                        console_events = None;
                        None
                    }
                },
            };
            if let Some(wanted) = requested {
                level_tx.send_modify(|level| *level = escalate(*level, wanted));
            }
            if level_tx.is_closed() {
                return;
            }
        }
    });
    StopSignals {
        level: level_rx,
        pending,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stop_levels_only_escalate() {
        let soon = Instant::now() + Duration::from_secs(1);
        let later = soon + Duration::from_secs(1);
        assert_eq!(escalate(None, StopLevel::Drain), Some(StopLevel::Drain));
        assert_eq!(
            escalate(Some(StopLevel::Drain), StopLevel::Now),
            Some(StopLevel::Now)
        );
        assert_eq!(
            escalate(Some(StopLevel::Now), StopLevel::Drain),
            Some(StopLevel::Now)
        );
        assert_eq!(
            escalate(
                Some(StopLevel::Now),
                StopLevel::EndSession { deadline: later }
            ),
            Some(StopLevel::EndSession { deadline: later })
        );
        assert_eq!(
            escalate(
                Some(StopLevel::EndSession { deadline: later }),
                StopLevel::Now
            ),
            Some(StopLevel::EndSession { deadline: later })
        );
        assert_eq!(
            escalate(
                Some(StopLevel::EndSession { deadline: later }),
                StopLevel::EndSession { deadline: soon }
            ),
            Some(StopLevel::EndSession { deadline: soon }),
            "the earliest deadline wins"
        );
    }

    #[tokio::test]
    async fn end_session_requests_are_held_until_acknowledged() {
        let (_admin_tx, admin_rx) = watch::channel(None);
        let (tx, rx) = mpsc::unbounded_channel();
        let signals = follow_signals(
            admin_rx,
            EndSessionSources {
                requests: rx,
                budget: Duration::from_secs(4),
                console: false,
            },
        );
        let mut level = signals.level();
        let deadline = std::time::Instant::now() + Duration::from_secs(4);
        let (request, waiter) = EndSession::with_waiter(deadline);
        tx.send(request).unwrap();
        let got = *level.wait_for(Option::is_some).await.unwrap();
        let Some(StopLevel::EndSession { deadline: at }) = got else {
            panic!("{got:?}")
        };
        assert!(
            at <= Instant::now() + Duration::from_secs(4)
                && at > Instant::now() + Duration::from_secs(3)
        );
        let waiting = tokio::task::spawn_blocking(move || waiter.wait_until(deadline));
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!waiting.is_finished(), "held until the shutdown is done");
        signals.acknowledge_end_session();
        assert!(waiting.await.unwrap());
    }
}
