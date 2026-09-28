//! Thin OS wrappers.

/// Creation time of a live process (Windows FILETIME ticks), or `None` if it is not running
/// or not accessible.
pub fn process_creation_time(pid: u32) -> Option<u64> {
    imp::process_creation_time(pid)
}

/// Whether the process `pid` created at `created` has not exited yet. Unlike
/// [`process_creation_time`], an exited process whose object is kept alive by an open handle
/// counts as gone.
pub fn process_is_running(pid: u32, created: u64) -> bool {
    imp::process_is_running(pid, created)
}

/// Terminates `pid` only if it is still the same process (same creation time).
/// Returns `Ok(true)` when it was terminated, `Ok(false)` when it no longer exists or the
/// PID now belongs to another process.
pub fn terminate_if_same(pid: u32, created: u64) -> std::io::Result<bool> {
    imp::terminate_if_same(pid, created)
}

#[cfg(windows)]
pub(crate) use imp::{OwnedProcess, TreeJob, handle_creation_time, process_parents};

#[cfg(windows)]
mod imp {
    use windows::Win32::Foundation::{
        CloseHandle, ERROR_INVALID_PARAMETER, FILETIME, HANDLE, INVALID_HANDLE_VALUE,
        WAIT_OBJECT_0, WAIT_TIMEOUT,
    };
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };
    use windows::Win32::System::IO::{
        CreateIoCompletionPort, GetQueuedCompletionStatus, OVERLAPPED,
    };
    use windows::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, IsProcessInJob,
        JOBOBJECT_ASSOCIATE_COMPLETION_PORT, JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
        JOBOBJECT_BASIC_PROCESS_ID_LIST, JobObjectAssociateCompletionPortInformation,
        JobObjectBasicAccountingInformation, JobObjectBasicProcessIdList,
        QueryInformationJobObject, SetInformationJobObject, TerminateJobObject,
    };
    use windows::Win32::System::SystemServices::JOB_OBJECT_MSG_NEW_PROCESS;
    use windows::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_QUOTA,
        PROCESS_SYNCHRONIZE, PROCESS_TERMINATE, TerminateProcess, WaitForSingleObject,
    };
    use windows::core::BOOL;

    /// Exit code given to processes the supervisor terminates.
    const TERMINATED_EXIT_CODE: u32 = 1;

    /// `(pid, parent pid)` of every process on the system, from one Toolhelp snapshot.
    ///
    /// The parent PID is the PID the parent had when the child was created; the parent may
    /// have exited since and the PID may have been reused, so callers must check creation
    /// times before treating the entry as a child (see `crate::orphans`).
    pub(crate) fn process_parents() -> std::io::Result<Vec<(u32, u32)>> {
        // SAFETY: plain snapshot creation; the handle is closed below.
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }
            .map_err(std::io::Error::other)?;
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut out = Vec::new();
        // SAFETY: `entry` is a live, correctly sized buffer; `snapshot` is valid until closed.
        let mut next = unsafe { Process32FirstW(snapshot, &mut entry) };
        while next.is_ok() {
            out.push((entry.th32ProcessID, entry.th32ParentProcessID));
            // SAFETY: as above.
            next = unsafe { Process32NextW(snapshot, &mut entry) };
        }
        // SAFETY: the snapshot handle is owned here and closed exactly once.
        let _ = unsafe { CloseHandle(snapshot) };
        Ok(out)
    }

    /// An open handle to a process whose creation time was read through that same handle.
    /// While the handle is open the PID cannot be given to another process, so the pair
    /// `(pid, created)` stays unambiguous.
    pub(crate) struct OwnedProcess {
        handle: HANDLE,
        pub(crate) pid: u32,
        pub(crate) created: u64,
    }

    // SAFETY: the handle is owned by this value and closed once on drop; process handles may
    // be used from any thread.
    unsafe impl Send for OwnedProcess {}

    impl std::fmt::Debug for OwnedProcess {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("OwnedProcess")
                .field("pid", &self.pid)
                .field("created", &self.created)
                .finish()
        }
    }

    impl OwnedProcess {
        /// Opens `pid` with the rights needed to inspect, terminate and put it in a job.
        /// `None` when there is no such process or it cannot be opened (another user's or an
        /// elevated process).
        pub(crate) fn open(pid: u32) -> Option<Self> {
            let access = PROCESS_QUERY_LIMITED_INFORMATION
                | PROCESS_TERMINATE
                | PROCESS_SET_QUOTA
                | PROCESS_SYNCHRONIZE;
            // SAFETY: OpenProcess has no memory-safety preconditions; the handle is owned by
            // the returned value (or closed right away).
            let handle = unsafe { OpenProcess(access, false, pid) }.ok()?;
            match creation_time_of(handle) {
                Some(created) => Some(Self {
                    handle,
                    pid,
                    created,
                }),
                None => {
                    // SAFETY: the handle was just opened and is closed exactly once.
                    let _ = unsafe { CloseHandle(handle) };
                    None
                }
            }
        }

        /// Whether the process has not exited yet.
        pub(crate) fn is_running(&self) -> bool {
            // SAFETY: valid handle with SYNCHRONIZE access; a zero timeout only polls.
            unsafe { WaitForSingleObject(self.handle, 0) == WAIT_TIMEOUT }
        }

        /// Starts terminating the process (the kernel finishes it asynchronously).
        pub(crate) fn terminate(&self) -> std::io::Result<()> {
            // SAFETY: valid handle with PROCESS_TERMINATE access.
            unsafe { TerminateProcess(self.handle, TERMINATED_EXIT_CODE) }
                .map_err(std::io::Error::other)
        }

        /// Waits up to `timeout` for the process to exit; `true` once it has.
        pub(crate) fn wait_exit(&self, timeout: std::time::Duration) -> bool {
            let millis = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX - 1);
            // SAFETY: valid handle with SYNCHRONIZE access.
            unsafe { WaitForSingleObject(self.handle, millis) == WAIT_OBJECT_0 }
        }
    }

    impl Drop for OwnedProcess {
        fn drop(&mut self) {
            // SAFETY: the handle is owned by this value and closed exactly once.
            let _ = unsafe { CloseHandle(self.handle) };
        }
    }

    fn creation_time_of(handle: HANDLE) -> Option<u64> {
        handle_creation_time(handle).ok()
    }

    /// Creation time of the process behind `handle` (which needs query access). Works for a
    /// process that has already exited, as long as the handle is open.
    pub(crate) fn handle_creation_time(handle: HANDLE) -> std::io::Result<u64> {
        let mut creation = FILETIME::default();
        let mut exit = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();
        // SAFETY: the handle is valid for the duration of the call and all out-pointers
        // point to live stack values.
        unsafe { GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) }
            .map_err(std::io::Error::other)?;
        Ok(((creation.dwHighDateTime as u64) << 32) | creation.dwLowDateTime as u64)
    }

    pub fn process_creation_time(pid: u32) -> Option<u64> {
        // SAFETY: OpenProcess has no memory-safety preconditions; the handle is closed below.
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?;
        let created = creation_time_of(handle);
        // SAFETY: handle came from OpenProcess and is closed exactly once.
        let _ = unsafe { CloseHandle(handle) };
        created
    }

    pub fn process_is_running(pid: u32, created: u64) -> bool {
        // SAFETY: OpenProcess has no memory-safety preconditions; the handle is closed below.
        let Ok(handle) = (unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                false,
                pid,
            )
        }) else {
            return false;
        };
        // SAFETY: valid process handle with SYNCHRONIZE access; a zero timeout only polls.
        let running = creation_time_of(handle) == Some(created)
            && unsafe { WaitForSingleObject(handle, 0) } == WAIT_TIMEOUT;
        // SAFETY: handle came from OpenProcess and is closed exactly once.
        let _ = unsafe { CloseHandle(handle) };
        running
    }

    pub fn terminate_if_same(pid: u32, created: u64) -> std::io::Result<bool> {
        // SAFETY: see process_creation_time.
        let Ok(handle) = (unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE,
                false,
                pid,
            )
        }) else {
            return Ok(false);
        };
        let result = if creation_time_of(handle) == Some(created) {
            // SAFETY: valid process handle with PROCESS_TERMINATE access.
            unsafe { TerminateProcess(handle, 1) }
                .map(|_| true)
                .map_err(std::io::Error::other)
        } else {
            Ok(false)
        };
        // SAFETY: handle came from OpenProcess and is closed exactly once.
        let _ = unsafe { CloseHandle(handle) };
        result
    }

    /// A job holding every process of one supervised tree, used to learn when the whole tree
    /// is gone.
    ///
    /// process-wrap's own job (which carries `KILL_ON_JOB_CLOSE` and receives
    /// `TerminateJobObject`) is not reachable from outside, and its `wait` returns once the
    /// main process has exited. The child is put into this job while it is still suspended,
    /// before process-wrap assigns it to its job, so that job becomes nested in this one and
    /// every descendant belongs to both.
    pub(crate) struct TreeJob {
        job: HANDLE,
        port: HANDLE,
    }

    // SAFETY: the handles are owned by this value, closed once on drop, and the job and port
    // APIs used here may be called from any thread.
    unsafe impl Send for TreeJob {}
    // SAFETY: see above; no method mutates the value.
    unsafe impl Sync for TreeJob {}

    impl std::fmt::Debug for TreeJob {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("TreeJob").finish_non_exhaustive()
        }
    }

    impl TreeJob {
        /// Creates the job with a completion port and puts `process` (still suspended) in it.
        pub(crate) fn for_process(process: HANDLE) -> std::io::Result<Self> {
            let tree = Self::new()?;
            // SAFETY: both handles are valid; the process is suspended and not yet in a job.
            unsafe { AssignProcessToJobObject(tree.job, process) }
                .map_err(std::io::Error::other)?;
            Ok(tree)
        }

        /// Puts a running process in the job (nested under any job it already belongs to).
        /// Processes it starts afterwards belong to the job from their creation on.
        pub(crate) fn assign(&self, process: &OwnedProcess) -> std::io::Result<()> {
            // SAFETY: both handles are valid; the process handle has PROCESS_SET_QUOTA and
            // PROCESS_TERMINATE access as the call requires.
            unsafe { AssignProcessToJobObject(self.job, process.handle) }
                .map_err(std::io::Error::other)
        }

        /// Whether `process` already belongs to this job (e.g. it was started by a member).
        pub(crate) fn contains(&self, process: &OwnedProcess) -> std::io::Result<bool> {
            let mut result = BOOL::default();
            // SAFETY: valid handles; `result` is a live out-value.
            unsafe { IsProcessInJob(process.handle, Some(self.job), &mut result) }
                .map_err(std::io::Error::other)?;
            Ok(result.as_bool())
        }

        /// Whether the process `pid` created at `created` belongs to this job. `Ok(false)` when no
        /// such process exists (any more); opening it needs query access only, so a process
        /// running under a restricted token (a sandbox) can be checked too.
        pub(crate) fn contains_process(&self, pid: u32, created: u64) -> std::io::Result<bool> {
            // SAFETY: OpenProcess has no memory-safety preconditions; the handle is closed below.
            let handle = match unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }
            {
                Ok(handle) => handle,
                // The PID names no process (any more).
                Err(e) if e.code() == ERROR_INVALID_PARAMETER.to_hresult() => return Ok(false),
                Err(e) => return Err(std::io::Error::other(e)),
            };
            let member = handle_creation_time(handle).and_then(|actual| {
                if actual != created {
                    return Ok(false);
                }
                let mut result = BOOL::default();
                // SAFETY: valid handles; `result` is a live out-value.
                unsafe { IsProcessInJob(handle, Some(self.job), &mut result) }
                    .map_err(std::io::Error::other)?;
                Ok(result.as_bool())
            });
            // SAFETY: the handle came from OpenProcess and is closed exactly once.
            let _ = unsafe { CloseHandle(handle) };
            member
        }

        /// Terminates every process of the job (asynchronously; see [`wait_empty`](Self::wait_empty)).
        pub(crate) fn terminate(&self) -> std::io::Result<()> {
            // SAFETY: valid job handle created by this value with full access.
            unsafe { TerminateJobObject(self.job, TERMINATED_EXIT_CODE) }
                .map_err(std::io::Error::other)
        }

        /// Creates an empty job with a completion port.
        pub(crate) fn new() -> std::io::Result<Self> {
            // SAFETY: plain object creation; ownership of the handle moves into `TreeJob` below
            // (or it is closed right away on failure).
            let job = unsafe { CreateJobObjectW(None, None) }.map_err(std::io::Error::other)?;
            // SAFETY: as above.
            let port = match unsafe { CreateIoCompletionPort(INVALID_HANDLE_VALUE, None, 0, 1) } {
                Ok(port) => port,
                Err(e) => {
                    // SAFETY: `job` was just created and is closed exactly once.
                    let _ = unsafe { CloseHandle(job) };
                    return Err(std::io::Error::other(e));
                }
            };
            let tree = TreeJob { job, port };
            let associate = JOBOBJECT_ASSOCIATE_COMPLETION_PORT {
                CompletionKey: job.0 as _,
                CompletionPort: port,
            };
            // SAFETY: `associate` lives for the call and its size is passed along.
            unsafe {
                SetInformationJobObject(
                    job,
                    JobObjectAssociateCompletionPortInformation,
                    (&associate as *const JOBOBJECT_ASSOCIATE_COMPLETION_PORT).cast(),
                    std::mem::size_of::<JOBOBJECT_ASSOCIATE_COMPLETION_PORT>() as u32,
                )
            }
            .map_err(std::io::Error::other)?;
            Ok(tree)
        }

        /// Number of processes of the tree that are still alive.
        pub(crate) fn active_processes(&self) -> std::io::Result<u32> {
            let mut info = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
            // SAFETY: `info` is a valid out-buffer of the size passed.
            unsafe {
                QueryInformationJobObject(
                    Some(self.job),
                    JobObjectBasicAccountingInformation,
                    (&mut info as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast(),
                    std::mem::size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                    None,
                )
            }
            .map_err(std::io::Error::other)?;
            Ok(info.ActiveProcesses)
        }

        /// Ids of the processes the job counts as active.
        pub(crate) fn process_ids(&self) -> std::io::Result<Vec<u32>> {
            let header = std::mem::size_of::<JOBOBJECT_BASIC_PROCESS_ID_LIST>()
                - std::mem::size_of::<usize>();
            // Room for what the job had a moment ago; asked again with more when it grew.
            let mut room = self.active_processes()? as usize + 1;
            loop {
                let len = header + room * std::mem::size_of::<usize>();
                let mut buf = vec![0usize; len.div_ceil(std::mem::size_of::<usize>())];
                // SAFETY: `buf` is a writable, aligned buffer of at least `len` bytes.
                let queried = unsafe {
                    QueryInformationJobObject(
                        Some(self.job),
                        JobObjectBasicProcessIdList,
                        buf.as_mut_ptr().cast(),
                        len as u32,
                        None,
                    )
                };
                // SAFETY: the buffer starts with the list's header (written by the query, or
                // zero when it failed).
                let list = unsafe { &*buf.as_ptr().cast::<JOBOBJECT_BASIC_PROCESS_ID_LIST>() };
                let (assigned, listed) = (
                    list.NumberOfAssignedProcesses as usize,
                    list.NumberOfProcessIdsInList as usize,
                );
                if queried.is_ok() && listed >= assigned {
                    // SAFETY: `listed` ids follow the header inside the buffer.
                    let ids = unsafe {
                        std::slice::from_raw_parts(
                            buf.as_ptr().cast::<u8>().add(header).cast::<usize>(),
                            listed,
                        )
                    };
                    return Ok(ids.iter().map(|&id| id as u32).collect());
                }
                if let Err(e) = queried
                    && assigned <= room
                {
                    return Err(std::io::Error::other(e));
                }
                room = assigned.max(room * 2);
            }
        }

        /// Ids of the processes the job reported as created since the port was last read
        /// (`JOB_OBJECT_MSG_NEW_PROCESS`). Reads what is queued without waiting.
        fn created_since_last_read(&self) -> Vec<u32> {
            let mut out = Vec::new();
            loop {
                let (mut code, mut key) = (0u32, 0usize);
                let mut overlapped: *mut OVERLAPPED = std::ptr::null_mut();
                // SAFETY: all out-pointers point to live locals; a zero timeout only polls.
                let got = unsafe {
                    GetQueuedCompletionStatus(self.port, &mut code, &mut key, &mut overlapped, 0)
                };
                if got.is_err() {
                    return out;
                }
                if code == JOB_OBJECT_MSG_NEW_PROCESS {
                    // For job notifications the "overlapped" value carries the process id.
                    out.push(overlapped as usize as u32);
                }
            }
        }

        /// Terminates every process of the tree and waits until each of them has exited
        /// (`Ok(true)`), or `timeout` passed (`Ok(false)`).
        ///
        /// The job's process count drops to zero as soon as `TerminateJobObject` returns, while
        /// the processes are still being torn down (and still hold their files and folders);
        /// only a process's own object tells when it is gone. So the processes are opened
        /// before the termination — the ones the job lists, and the ones it reported as created
        /// (which also covers a process started right before the termination) — and each is
        /// waited for. Opening checks that the process belongs to this job, so a reused id
        /// never makes it wait for an unrelated process.
        pub(crate) fn terminate_and_wait(
            &self,
            timeout: std::time::Duration,
        ) -> std::io::Result<bool> {
            let deadline = std::time::Instant::now() + timeout;
            let mut held: Vec<OwnedProcess> = Vec::new();
            let hold = |pid: u32, held: &mut Vec<OwnedProcess>| {
                if held.iter().any(|p| p.pid == pid) {
                    return;
                }
                if let Some(process) = OwnedProcess::open(pid)
                    && self.contains(&process).unwrap_or(false)
                {
                    held.push(process);
                }
            };
            for pid in self.process_ids()? {
                hold(pid, &mut held);
            }
            for pid in self.created_since_last_read() {
                hold(pid, &mut held);
            }
            self.terminate()?;
            for pid in self.created_since_last_read() {
                hold(pid, &mut held);
            }
            for process in &held {
                if !process.wait_exit(deadline.saturating_duration_since(std::time::Instant::now()))
                {
                    return Ok(false);
                }
            }
            Ok(true)
        }

        /// Blocks until no process of the tree is alive (`Ok(true)`) or `timeout` passed
        /// (`Ok(false)`). Every job notification (a process ended, the count reached zero)
        /// wakes the wait, so no polling interval is involved.
        pub(crate) fn wait_empty(&self, timeout: std::time::Duration) -> std::io::Result<bool> {
            let deadline = std::time::Instant::now() + timeout;
            loop {
                if self.active_processes()? == 0 {
                    return Ok(true);
                }
                let left = deadline.saturating_duration_since(std::time::Instant::now());
                if left.is_zero() {
                    return Ok(false);
                }
                let millis = u32::try_from(left.as_millis()).unwrap_or(u32::MAX - 1);
                let (mut code, mut key) = (0u32, 0usize);
                let mut overlapped: *mut OVERLAPPED = std::ptr::null_mut();
                // SAFETY: all out-pointers point to live locals. A timeout is reported as an
                // error, after which the loop checks the count and the deadline again.
                let _ = unsafe {
                    GetQueuedCompletionStatus(
                        self.port,
                        &mut code,
                        &mut key,
                        &mut overlapped,
                        millis,
                    )
                };
            }
        }
    }

    impl Drop for TreeJob {
        fn drop(&mut self) {
            // SAFETY: both handles are owned by this value and closed exactly once.
            unsafe {
                let _ = CloseHandle(self.port);
                let _ = CloseHandle(self.job);
            }
        }
    }
}

#[cfg(unix)]
mod imp {
    // On Unix the ledger stores the process start time in clock ticks since boot
    // (field 22 of /proc/<pid>/stat) where available; elsewhere the sweep is skipped.
    pub fn process_creation_time(pid: u32) -> Option<u64> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let after_comm = stat.rsplit_once(')')?.1;
        after_comm.split_whitespace().nth(19)?.parse().ok()
    }

    pub fn process_is_running(pid: u32, created: u64) -> bool {
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            return false;
        };
        let Some((_, after_comm)) = stat.rsplit_once(')') else {
            return false;
        };
        let mut fields = after_comm.split_whitespace();
        let state = fields.next();
        let start = fields.nth(18).and_then(|f| f.parse::<u64>().ok());
        start == Some(created) && state != Some("Z") && state != Some("X")
    }

    pub fn terminate_if_same(pid: u32, created: u64) -> std::io::Result<bool> {
        if process_creation_time(pid) != Some(created) {
            return Ok(false);
        }
        let status = std::process::Command::new("kill")
            .arg("-9")
            .arg(pid.to_string())
            .status()?;
        Ok(status.success())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn own_process_has_a_creation_time() {
        let pid = std::process::id();
        let created = process_creation_time(pid);
        assert!(created.is_some());
        // A wrong creation time must never terminate the process.
        assert!(!terminate_if_same(pid, created.unwrap() + 1).unwrap());
    }

    #[test]
    fn own_process_is_running() {
        let pid = std::process::id();
        let created = process_creation_time(pid).unwrap();
        assert!(process_is_running(pid, created));
        assert!(
            !process_is_running(pid, created + 1),
            "another creation time is another process"
        );
    }

    #[test]
    fn missing_process_is_not_found() {
        // PIDs are multiples of 4 on Windows; 3 is never valid there and unlikely elsewhere.
        assert_eq!(process_creation_time(3), None);
        assert!(!terminate_if_same(3, 42).unwrap());
    }
}
