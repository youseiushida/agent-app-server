//! The end of the Windows session (sign-out, shutdown, reboot).
//!
//! Windows tells applications through `WM_QUERYENDSESSION` / `WM_ENDSESSION`, which are sent
//! only to top-level windows (message-only windows do not get them). Neither the daemon (a
//! console program started without a console window) nor the watchdog (no window at all)
//! has one, so [`EndSessionWindow`] creates a hidden top-level window on its own thread.
//!
//! On `WM_ENDSESSION` (the session really ends) the window procedure hands an [`EndSession`]
//! with an absolute deadline to the async side and blocks — returning from `WM_ENDSESSION`
//! allows Windows to terminate the process at any moment — until the receiver acknowledges
//! that its staged shutdown is done, or the deadline passes. The deadline is
//! `policy.end_session_deadline` after the message arrived, which fits the time Windows gives
//! an application to return from the message.
//!
//! Console runs also get `CTRL_CLOSE_EVENT` / `CTRL_LOGOFF_EVENT` / `CTRL_SHUTDOWN_EVENT`
//! (registered with `SetConsoleCtrlHandler` through `tokio::signal::windows`); those become an
//! [`EndSession`] without a waiter (the console handler keeps the process alive until Windows'
//! own timeout).

use std::sync::mpsc;
use std::time::Instant;

/// The session is ending: shut down before `deadline`, then [`ack`](EndSession::ack).
#[derive(Debug)]
pub struct EndSession {
    pub deadline: Instant,
    ack: Option<mpsc::Sender<()>>,
}

/// Held by whoever must wait for an [`EndSession`] to be acknowledged (the window procedure).
#[derive(Debug)]
pub struct AckWaiter(mpsc::Receiver<()>);

impl EndSession {
    /// A request and the waiter its acknowledgement releases.
    pub fn with_waiter(deadline: Instant) -> (Self, AckWaiter) {
        let (tx, rx) = mpsc::channel();
        (
            Self {
                deadline,
                ack: Some(tx),
            },
            AckWaiter(rx),
        )
    }

    /// A request nobody waits for (console events, the watchdog's control line).
    pub fn unobserved(deadline: Instant) -> Self {
        Self {
            deadline,
            ack: None,
        }
    }

    /// Time left until the deadline (zero once it has passed).
    pub fn remaining(&self) -> std::time::Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }

    /// The shutdown is done (or given up): the waiter may return.
    pub fn ack(self) {
        if let Some(tx) = self.ack {
            // The waiter gave up at its deadline already: nothing left to release.
            let _ = tx.send(());
        }
    }
}

impl AckWaiter {
    /// Blocks until the request is acknowledged (`true`) or `deadline` passes (`false`). A
    /// request dropped without an acknowledgement counts as acknowledged: its receiver is gone.
    pub fn wait_until(self, deadline: Instant) -> bool {
        let timeout = deadline.saturating_duration_since(Instant::now());
        !matches!(
            self.0.recv_timeout(timeout),
            Err(mpsc::RecvTimeoutError::Timeout)
        )
    }
}

#[cfg(windows)]
pub use imp::EndSessionWindow;

#[cfg(windows)]
mod imp {
    use std::ffi::c_void;
    use std::sync::OnceLock;
    use std::time::{Duration, Instant};

    use tokio::sync::mpsc::UnboundedSender;
    use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::WindowsAndMessaging::{
        CREATESTRUCTW, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
        GWLP_USERDATA, GetMessageW, GetWindowLongPtrW, MSG, PostMessageW, PostQuitMessage,
        RegisterClassExW, SetWindowLongPtrW, TranslateMessage, WINDOW_EX_STYLE, WM_APP, WM_CLOSE,
        WM_DESTROY, WM_ENDSESSION, WM_NCCREATE, WM_QUERYENDSESSION, WNDCLASSEXW, WS_OVERLAPPED,
    };
    use windows::core::{PCWSTR, w};

    use super::EndSession;

    /// Posted by [`EndSessionWindow`]'s `Drop` to end the window's thread.
    const WM_QUIT_LISTENER: u32 = WM_APP + 1;
    const CLASS_NAME: PCWSTR = w!("AgentAppServerEndSession");

    /// State of one window, owned by its thread and reachable from the window procedure
    /// through `GWLP_USERDATA`.
    struct WindowState {
        budget: Duration,
        tx: UnboundedSender<EndSession>,
    }

    /// A hidden top-level window that turns `WM_ENDSESSION` into [`EndSession`] requests.
    pub struct EndSessionWindow {
        hwnd: isize,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    fn register_class() -> Result<(), String> {
        static REGISTERED: OnceLock<Result<(), String>> = OnceLock::new();
        REGISTERED
            .get_or_init(|| {
                // SAFETY: GetModuleHandleW(None) returns the handle of this executable.
                let module = unsafe { GetModuleHandleW(None) }.map_err(|e| e.to_string())?;
                let class = WNDCLASSEXW {
                    cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                    lpfnWndProc: Some(wnd_proc),
                    hInstance: HINSTANCE(module.0),
                    lpszClassName: CLASS_NAME,
                    ..Default::default()
                };
                // SAFETY: `class` is fully initialised and outlives the call; the class name is
                // a static string.
                if unsafe { RegisterClassExW(&class) } == 0 {
                    return Err(std::io::Error::last_os_error().to_string());
                }
                Ok(())
            })
            .clone()
    }

    impl EndSessionWindow {
        /// Creates the window on a new thread. Every `WM_ENDSESSION` that ends the session
        /// sends an [`EndSession`] with the deadline `budget` after the message arrived to
        /// `tx`, and blocks the window procedure until it is acknowledged or the deadline
        /// passes.
        pub fn start(budget: Duration, tx: UnboundedSender<EndSession>) -> std::io::Result<Self> {
            register_class().map_err(|e| {
                std::io::Error::other(format!("registering the end-session window class: {e}"))
            })?;
            let (created_tx, created_rx) = std::sync::mpsc::channel::<Result<isize, String>>();
            let thread = std::thread::Builder::new()
                .name("aas-end-session".into())
                .spawn(move || {
                    let state = Box::into_raw(Box::new(WindowState { budget, tx }));
                    // SAFETY: GetModuleHandleW(None) returns the handle of this executable.
                    let module = match unsafe { GetModuleHandleW(None) } {
                        Ok(m) => m,
                        Err(e) => {
                            // SAFETY: `state` came from Box::into_raw above and was not shared.
                            drop(unsafe { Box::from_raw(state) });
                            let _ = created_tx.send(Err(e.to_string()));
                            return;
                        }
                    };
                    // SAFETY: the class is registered; `state` stays valid until the message loop
                    // below has ended (it is freed after it). A zero-sized window without
                    // WS_VISIBLE is never shown.
                    let created = unsafe {
                        CreateWindowExW(
                            WINDOW_EX_STYLE(0),
                            CLASS_NAME,
                            w!("agent-app-server"),
                            WS_OVERLAPPED,
                            0,
                            0,
                            0,
                            0,
                            None,
                            None,
                            Some(HINSTANCE(module.0)),
                            Some(state as *const c_void),
                        )
                    };
                    let hwnd = match created {
                        Ok(hwnd) => hwnd,
                        Err(e) => {
                            // SAFETY: the window was not created, so nothing else refers to `state`.
                            drop(unsafe { Box::from_raw(state) });
                            let _ = created_tx.send(Err(e.to_string()));
                            return;
                        }
                    };
                    let _ = created_tx.send(Ok(hwnd.0 as isize));
                    let mut msg = MSG::default();
                    // SAFETY: standard message loop on the thread that owns the window. GetMessageW
                    // returns 0 on WM_QUIT and -1 on failure; both end the loop.
                    while unsafe { GetMessageW(&mut msg, None, 0, 0) }.0 > 0 {
                        // SAFETY: `msg` was filled by GetMessageW.
                        unsafe {
                            let _ = TranslateMessage(&msg);
                            DispatchMessageW(&msg);
                        }
                    }
                    // SAFETY: the window is destroyed (or its thread is ending), so the window
                    // procedure no longer runs for it; `state` is freed exactly once.
                    drop(unsafe { Box::from_raw(state) });
                })?;
            match created_rx.recv() {
                Ok(Ok(hwnd)) => Ok(Self {
                    hwnd,
                    thread: Some(thread),
                }),
                Ok(Err(e)) => {
                    let _ = thread.join();
                    Err(std::io::Error::other(format!(
                        "creating the end-session window: {e}"
                    )))
                }
                Err(_) => {
                    let _ = thread.join();
                    Err(std::io::Error::other(
                        "the end-session window thread ended before creating its window",
                    ))
                }
            }
        }

        /// The window handle (tests send it messages).
        pub fn hwnd(&self) -> isize {
            self.hwnd
        }
    }

    impl Drop for EndSessionWindow {
        fn drop(&mut self) {
            // SAFETY: posting to a window handle is safe even if the window is gone (the call
            // then fails).
            let posted = unsafe {
                PostMessageW(
                    Some(HWND(self.hwnd as *mut c_void)),
                    WM_QUIT_LISTENER,
                    WPARAM(0),
                    LPARAM(0),
                )
            };
            if let (Ok(()), Some(thread)) = (posted, self.thread.take()) {
                let _ = thread.join();
            }
        }
    }

    fn state_of(hwnd: HWND) -> Option<&'static WindowState> {
        // SAFETY: GWLP_USERDATA holds the pointer stored at WM_NCCREATE (or 0 before), which
        // stays valid while the window exists.
        let ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const WindowState;
        // SAFETY: see above; the state outlives every message the window receives.
        unsafe { ptr.as_ref() }
    }

    fn end_session(state: &WindowState, lparam: LPARAM) {
        let deadline = Instant::now() + state.budget;
        let (request, waiter) = EndSession::with_waiter(deadline);
        tracing::warn!(flags = format!("{:#x}", lparam.0), budget = ?state.budget, "Windows is ending the session; shutting down");
        if state.tx.send(request).is_err() {
            tracing::error!("nobody handles the end of the session");
            return;
        }
        if !waiter.wait_until(deadline) {
            tracing::warn!("the end-session shutdown did not finish within its deadline");
        }
    }

    unsafe extern "system" fn wnd_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        match msg {
            WM_NCCREATE => {
                // SAFETY: for WM_NCCREATE, lparam points to the CREATESTRUCTW of this window,
                // whose lpCreateParams is the state pointer passed to CreateWindowExW.
                unsafe {
                    let create = lparam.0 as *const CREATESTRUCTW;
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, (*create).lpCreateParams as isize);
                    DefWindowProcW(hwnd, msg, wparam, lparam)
                }
            }
            WM_QUERYENDSESSION => {
                tracing::info!(
                    flags = format!("{:#x}", lparam.0),
                    "Windows asks whether the session may end"
                );
                // Never veto: the daemon has nothing the user must save.
                LRESULT(1)
            }
            WM_ENDSESSION => {
                // wparam == 0: the end of the session was cancelled after the query.
                if wparam.0 != 0
                    && let Some(state) = state_of(hwnd)
                {
                    end_session(state, lparam);
                }
                LRESULT(0)
            }
            // `taskkill` without /F and similar tools post WM_CLOSE to top-level windows; this
            // window is the process's end-session listener, not a way to stop it.
            WM_CLOSE => LRESULT(0),
            WM_QUIT_LISTENER => {
                // SAFETY: destroying our own window on its own thread.
                if let Err(e) = unsafe { DestroyWindow(hwnd) } {
                    tracing::error!(error = %e, "could not destroy the end-session window");
                    // SAFETY: ends this thread's message loop.
                    unsafe { PostQuitMessage(0) };
                }
                LRESULT(0)
            }
            WM_DESTROY => {
                // SAFETY: ends this thread's message loop.
                unsafe { PostQuitMessage(0) };
                LRESULT(0)
            }
            // SAFETY: default processing of every other message.
            _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use windows::Win32::UI::WindowsAndMessaging::{ENDSESSION_LOGOFF, SendMessageW};

        fn send(hwnd: isize, msg: u32, wparam: usize, lparam: isize) -> isize {
            // SAFETY: plain SendMessageW to a window of this process (or a gone one).
            unsafe {
                SendMessageW(
                    HWND(hwnd as *mut c_void),
                    msg,
                    Some(WPARAM(wparam)),
                    Some(LPARAM(lparam)),
                )
            }
            .0
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn end_session_messages_wait_for_the_shutdown() {
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            let window = EndSessionWindow::start(Duration::from_secs(5), tx).unwrap();
            let hwnd = window.hwnd();
            // The query is always answered with "yes".
            let answer = tokio::task::spawn_blocking(move || {
                send(hwnd, WM_QUERYENDSESSION, 0, ENDSESSION_LOGOFF as isize)
            });
            assert_eq!(answer.await.unwrap(), 1);
            // A cancelled end (wParam = 0) asks for nothing.
            tokio::task::spawn_blocking(move || {
                send(hwnd, WM_ENDSESSION, 0, ENDSESSION_LOGOFF as isize)
            })
            .await
            .unwrap();
            assert!(rx.try_recv().is_err());
            // WM_CLOSE does not end the listener.
            tokio::task::spawn_blocking(move || send(hwnd, WM_CLOSE, 0, 0))
                .await
                .unwrap();

            // The real end: the window procedure blocks until the shutdown acknowledges.
            let started = Instant::now();
            let sending = tokio::task::spawn_blocking(move || {
                send(hwnd, WM_ENDSESSION, 1, ENDSESSION_LOGOFF as isize);
                Instant::now()
            });
            let request = tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .unwrap()
                .unwrap();
            assert!(
                request.deadline > started + Duration::from_secs(4),
                "the deadline is the budget after the message"
            );
            tokio::time::sleep(Duration::from_millis(300)).await;
            assert!(
                !sending.is_finished(),
                "WM_ENDSESSION returns only after the acknowledgement"
            );
            let acked = Instant::now();
            request.ack();
            let returned = sending.await.unwrap();
            assert!(returned >= acked);
            assert!(returned - started < Duration::from_secs(4));
            drop(window);
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn an_unacknowledged_end_returns_at_the_deadline() {
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            let window = EndSessionWindow::start(Duration::from_millis(500), tx).unwrap();
            let hwnd = window.hwnd();
            let started = Instant::now();
            let sending = tokio::task::spawn_blocking(move || send(hwnd, WM_ENDSESSION, 1, 0));
            let request = rx.recv().await.unwrap();
            sending.await.unwrap();
            let waited = started.elapsed();
            assert!(
                waited >= Duration::from_millis(450) && waited < Duration::from_secs(3),
                "{waited:?}"
            );
            // Acknowledging late is harmless.
            request.ack();
            drop(window);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn waiters_are_released_by_the_ack_the_drop_or_the_deadline() {
        let (request, waiter) = EndSession::with_waiter(Instant::now() + Duration::from_secs(5));
        request.ack();
        assert!(waiter.wait_until(Instant::now() + Duration::from_secs(5)));
        let (request, waiter) = EndSession::with_waiter(Instant::now() + Duration::from_secs(5));
        drop(request);
        assert!(
            waiter.wait_until(Instant::now() + Duration::from_secs(5)),
            "a dropped request has no receiver left"
        );
        let (_request, waiter) = EndSession::with_waiter(Instant::now());
        assert!(!waiter.wait_until(Instant::now() + Duration::from_millis(50)));
        EndSession::unobserved(Instant::now()).ack();
    }
}
