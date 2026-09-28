//! Start at logon through Windows Task Scheduler (design.md §18.3).
//!
//! Two tasks run the watchdog (`agent-app-server-daemon.exe`) in the user's session, with the
//! user's rights (the agent CLIs read their credentials from the user profile, so a Windows
//! service running as another account would not work). No time limit, not stopped on
//! battery, one instance each:
//!
//! * [`TASK_NAME`] starts it at logon (and on `autostart install` or
//!   `schtasks /Run /TN agent-app-server`): an explicit start.
//! * [`KEEPALIVE_TASK_NAME`] starts it every `policy.autostart_keepalive_interval` with
//!   [`KEEPALIVE_ARG`]. Task Scheduler does not restart a task whose program exits with a
//!   non-zero code (measured by `tests/autostart.rs`), so this task brings the watchdog back
//!   after a crash of the watchdog itself. A keep-alive start ends at once when a watchdog runs
//!   or when the last one was stopped on purpose (see `crate::watchdog`).
//!
//! Task Scheduler is driven through its COM API (`ITaskService`, `ITaskFolder`,
//! `IRegisteredTask`). Its HRESULTs tell "the task does not exist" apart from every other
//! failure; nothing is read from the human-readable output of `schtasks`.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, bail};

use crate::config::Paths;

/// The logon task.
pub const TASK_NAME: &str = "agent-app-server";
/// The keep-alive task.
pub const KEEPALIVE_TASK_NAME: &str = "agent-app-server-keepalive";
/// The watchdog argument of a start by the keep-alive task.
pub const KEEPALIVE_ARG: &str = "--keepalive";

/// What a task runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskAction {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub working_dir: PathBuf,
}

/// When a task starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskTrigger {
    /// At the logon of the task's user.
    Logon,
    /// Every `every` (at least one minute, Task Scheduler's shortest repetition), for as long
    /// as the user is logged on.
    Every(Duration),
}

/// A task of the current user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskDefinition {
    pub name: String,
    pub description: String,
    pub trigger: TaskTrigger,
    pub action: TaskAction,
    /// `DOMAIN\user` of the principal.
    pub user: String,
    /// Task Scheduler's own "restart on failure" (interval, count). The tasks of
    /// `autostart install` leave it off: it restarts a task only when Task Scheduler cannot
    /// start its program, never when the program exits with a non-zero code (design.md §18.3,
    /// measured by `tests/autostart.rs`, which is why the setting exists here), and the
    /// keep-alive task covers a program that could not be started as well.
    pub restart_on_failure: Option<(Duration, u32)>,
}

/// State of a registered task (`IRegisteredTask`).
#[derive(Debug, Clone, PartialEq)]
pub struct TaskStatus {
    pub name: String,
    pub enabled: bool,
    /// `TASK_STATE`: unknown, disabled, queued, ready or running.
    pub state: &'static str,
    /// Last start (an OLE automation date in local time; `None` when it never ran).
    pub last_run: Option<f64>,
    /// `LastTaskResult`: the exit code of the last run, or an `SCHED_S_*` / `SCHED_E_*`
    /// HRESULT of Task Scheduler.
    pub last_result: i32,
    pub next_run: Option<f64>,
}

/// `SCHED_S_TASK_HAS_NOT_RUN` (schedule.h): the task has not run yet.
pub const SCHED_S_TASK_HAS_NOT_RUN: i32 = 0x0004_1303;
/// `SCHED_S_TASK_RUNNING`: the task is running.
pub const SCHED_S_TASK_RUNNING: i32 = 0x0004_1301;

impl TaskStatus {
    /// One line for `autostart status` and `doctor`.
    pub fn describe(&self) -> String {
        let when = |d: Option<f64>| d.map(format_ole_date).unwrap_or_else(|| "never".into());
        format!(
            "{}: {}{}, last run {}, last result {}, next run {}",
            self.name,
            self.state,
            if self.enabled { "" } else { " (disabled)" },
            when(self.last_run),
            describe_result(self.last_result),
            when(self.next_run)
        )
    }
}

/// `LastTaskResult` in words: an exit code, or a Task Scheduler HRESULT in hex.
pub fn describe_result(code: i32) -> String {
    match code {
        SCHED_S_TASK_HAS_NOT_RUN => "none (has not run)".into(),
        SCHED_S_TASK_RUNNING => "none (running)".into(),
        0..=0xFFFF => format!("exit code {code}"),
        other => format!("0x{:08X}", other as u32),
    }
}

/// An OLE automation date (days since 1899-12-30, local time) as `YYYY-MM-DD HH:MM:SS`.
pub fn format_ole_date(date: f64) -> String {
    let seconds = (date * 86_400.0).round() as i64;
    let days = seconds.div_euclid(86_400);
    let secs = seconds.rem_euclid(86_400);
    // Days from 1899-12-30 to 1970-01-01.
    let (y, m, d) = civil_from_days(days - 25_569);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}",
        secs / 3600,
        secs % 3600 / 60,
        secs % 60
    )
}

/// The proleptic Gregorian date of `z` days after 1970-01-01 (H. Hinnant's algorithm).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// An XML Schema duration (`PT5M`, `P1DT2H`) of whole seconds.
fn xs_duration(d: Duration) -> String {
    let total = d.as_secs();
    let (days, rest) = (total / 86_400, total % 86_400);
    let (h, m, s) = (rest / 3600, rest % 3600 / 60, rest % 60);
    let mut out = String::from("P");
    if days > 0 {
        out.push_str(&format!("{days}D"));
    }
    if rest > 0 || days == 0 {
        out.push('T');
        if h > 0 {
            out.push_str(&format!("{h}H"));
        }
        if m > 0 {
            out.push_str(&format!("{m}M"));
        }
        if s > 0 || rest == 0 {
            out.push_str(&format!("{s}S"));
        }
    }
    out
}

/// Quotes one argument for a Windows command line (the rules of `CommandLineToArgvW`, which
/// the Rust runtime of the watchdog follows too).
pub fn quote_arg(arg: &str) -> String {
    if !arg.is_empty() && !arg.contains([' ', '\t', '\n', '\u{b}', '"']) {
        return arg.to_owned();
    }
    let mut out = String::from("\"");
    let mut backslashes = 0usize;
    for c in arg.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                out.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            other => {
                out.extend(std::iter::repeat_n('\\', backslashes));
                out.push(other);
                backslashes = 0;
            }
        }
    }
    out.extend(std::iter::repeat_n('\\', backslashes * 2));
    out.push('"');
    out
}

/// The task definition (Task Scheduler schema 1.4).
pub fn task_xml(def: &TaskDefinition) -> String {
    let user = xml_escape(&def.user);
    let trigger = match def.trigger {
        TaskTrigger::Logon => format!(
            "    <LogonTrigger>\n      <Enabled>true</Enabled>\n      <UserId>{user}</UserId>\n    </LogonTrigger>\n"
        ),
        // A boundary in the past: the repetition runs from registration on.
        TaskTrigger::Every(every) => format!(
            "    <TimeTrigger>\n      <Repetition>\n        <Interval>{}</Interval>\n        <StopAtDurationEnd>false</StopAtDurationEnd>\n      </Repetition>\n      <StartBoundary>2000-01-01T00:00:00</StartBoundary>\n      <Enabled>true</Enabled>\n    </TimeTrigger>\n",
            xs_duration(every)
        ),
    };
    let args: Vec<String> = def
        .action
        .args
        .iter()
        .map(|a| quote_arg(&a.to_string_lossy()))
        .collect();
    let arguments = if args.is_empty() {
        String::new()
    } else {
        format!(
            "      <Arguments>{}</Arguments>\n",
            xml_escape(&args.join(" "))
        )
    };
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.4" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>{description}</Description>
    <URI>\{name}</URI>
  </RegistrationInfo>
  <Triggers>
{trigger}  </Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>{user}</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>LeastPrivilege</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>true</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <IdleSettings>
      <StopOnIdleEnd>false</StopOnIdleEnd>
      <RestartOnIdle>false</RestartOnIdle>
    </IdleSettings>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>false</Hidden>
    <RunOnlyIfIdle>false</RunOnlyIfIdle>
    <WakeToRun>false</WakeToRun>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>7</Priority>
{restart}  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{exe}</Command>
{arguments}      <WorkingDirectory>{wd}</WorkingDirectory>
    </Exec>
  </Actions>
</Task>
"#,
        restart = def
            .restart_on_failure
            .map(|(interval, count)| format!(
                "    <RestartOnFailure>\n      <Interval>{}</Interval>\n      <Count>{count}</Count>\n    </RestartOnFailure>\n",
                xs_duration(interval)
            ))
            .unwrap_or_default(),
        description = xml_escape(&def.description),
        name = xml_escape(&def.name),
        exe = xml_escape(&def.action.program.display().to_string()),
        wd = xml_escape(&def.action.working_dir.display().to_string()),
    )
}

/// `DOMAIN\user` of the current user.
pub fn current_user() -> anyhow::Result<String> {
    let user = std::env::var("USERNAME").context("USERNAME is not set")?;
    Ok(match std::env::var("USERDOMAIN") {
        Ok(domain) if !domain.is_empty() => format!("{domain}\\{user}"),
        _ => user,
    })
}

/// The two tasks `autostart install` registers for the watchdog at `daemon_exe`: the folders
/// are passed explicitly, so the tasks use the ones `install` used (flags, environment
/// variables or defaults) whatever the environment at logon.
pub fn definitions(
    daemon_exe: &Path,
    paths: &Paths,
    keepalive: Duration,
    user: &str,
) -> [TaskDefinition; 2] {
    let args = |extra: Option<&str>| -> Vec<OsString> {
        let mut args: Vec<OsString> = extra.into_iter().map(OsString::from).collect();
        args.extend([
            OsString::from("--config-dir"),
            paths.config_dir.clone().into_os_string(),
            OsString::from("--data-dir"),
            paths.data_dir.clone().into_os_string(),
        ]);
        args
    };
    let action = |extra| TaskAction {
        program: daemon_exe.to_path_buf(),
        args: args(extra),
        working_dir: paths.data_dir.clone(),
    };
    [
        TaskDefinition {
            name: TASK_NAME.into(),
            description: "agent-app-server: coding-agent daemon for the phone app (starts the watchdog at logon)".into(),
            trigger: TaskTrigger::Logon,
            action: action(None),
            user: user.to_owned(),
            restart_on_failure: None,
        },
        TaskDefinition {
            name: KEEPALIVE_TASK_NAME.into(),
            description: "agent-app-server: starts the watchdog again if it ended without being stopped".into(),
            trigger: TaskTrigger::Every(keepalive),
            action: action(Some(KEEPALIVE_ARG)),
            user: user.to_owned(),
            restart_on_failure: None,
        },
    ]
}

/// Registers `def` for the current user, replacing a task of the same name.
pub async fn register(def: &TaskDefinition) -> anyhow::Result<()> {
    let (name, xml) = (def.name.clone(), task_xml(def));
    blocking(move || scheduler::register(&name, &xml)).await
}

/// The task's state; `None` when no such task exists (any other failure is an error).
pub async fn query(name: &str) -> anyhow::Result<Option<TaskStatus>> {
    let name = name.to_owned();
    blocking(move || scheduler::query(&name)).await
}

/// Deletes the task; `false` when it did not exist.
pub async fn delete(name: &str) -> anyhow::Result<bool> {
    let name = name.to_owned();
    blocking(move || scheduler::delete(&name)).await
}

/// Starts the task now.
pub async fn run(name: &str) -> anyhow::Result<()> {
    let name = name.to_owned();
    blocking(move || scheduler::run(&name)).await
}

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> anyhow::Result<T> + Send + 'static,
) -> anyhow::Result<T> {
    tokio::task::spawn_blocking(f)
        .await
        .context("the Task Scheduler call panicked")?
}

/// `autostart install`: registers both tasks and starts the watchdog now.
pub async fn install(paths: &Paths, keepalive: Duration) -> anyhow::Result<()> {
    let daemon = crate::exe_dir()?.join("agent-app-server-daemon.exe");
    if !daemon.is_file() {
        bail!("{} not found next to this program", daemon.display());
    }
    std::fs::create_dir_all(&paths.data_dir)
        .with_context(|| format!("creating {}", paths.data_dir.display()))?;
    for def in definitions(&daemon, paths, keepalive, &current_user()?) {
        register(&def)
            .await
            .with_context(|| format!("registering the task {}", def.name))?;
    }
    run(TASK_NAME)
        .await
        .context("the tasks were registered but the watchdog could not be started")
}

/// `autostart uninstall`: removes both tasks (a missing one is fine). A running daemon keeps
/// running.
pub async fn uninstall() -> anyhow::Result<()> {
    let mut removed = false;
    for name in [TASK_NAME, KEEPALIVE_TASK_NAME] {
        removed |= delete(name)
            .await
            .with_context(|| format!("deleting the task {name}"))?;
    }
    if !removed {
        bail!("autostart is not installed");
    }
    Ok(())
}

/// Both tasks' states (`None` for a task that is not registered).
pub async fn status() -> anyhow::Result<[(&'static str, Option<TaskStatus>); 2]> {
    Ok([
        (TASK_NAME, query(TASK_NAME).await?),
        (KEEPALIVE_TASK_NAME, query(KEEPALIVE_TASK_NAME).await?),
    ])
}

#[cfg(windows)]
mod scheduler {
    use anyhow::Context;
    use windows::Win32::Foundation::{RPC_E_CHANGED_MODE, VARIANT_BOOL};
    use windows::Win32::System::Com::{
        CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
        CoUninitialize,
    };
    use windows::Win32::System::TaskScheduler::{
        IRegisteredTask, ITaskFolder, ITaskService, TASK_CREATE_OR_UPDATE,
        TASK_LOGON_INTERACTIVE_TOKEN, TASK_STATE_DISABLED, TASK_STATE_QUEUED, TASK_STATE_READY,
        TASK_STATE_RUNNING, TaskScheduler,
    };
    use windows::Win32::System::Variant::VARIANT;
    use windows::core::{BSTR, HRESULT};

    use super::TaskStatus;

    /// `HRESULT_FROM_WIN32(ERROR_FILE_NOT_FOUND)`: what `ITaskFolder::GetTask` and
    /// `ITaskFolder::DeleteTask` return for a task that does not exist.
    const TASK_NOT_FOUND: HRESULT = HRESULT(0x8007_0002_u32 as i32);

    /// COM on this thread for as long as it lives.
    struct Com {
        uninitialize: bool,
    }

    impl Com {
        fn init() -> anyhow::Result<Self> {
            // SAFETY: plain COM initialization of the calling (blocking pool) thread.
            let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
            if hr == RPC_E_CHANGED_MODE {
                // Already initialized in another apartment by someone else: usable as it is.
                return Ok(Self {
                    uninitialize: false,
                });
            }
            hr.ok().context("initializing COM")?;
            Ok(Self { uninitialize: true })
        }
    }

    impl Drop for Com {
        fn drop(&mut self) {
            if self.uninitialize {
                // SAFETY: balances the successful CoInitializeEx of this thread.
                unsafe { CoUninitialize() };
            }
        }
    }

    /// Runs `f` with the root task folder of the current user's connection.
    fn with_root<T>(f: impl FnOnce(&ITaskFolder) -> anyhow::Result<T>) -> anyhow::Result<T> {
        let _com = Com::init()?;
        // SAFETY: COM is initialized on this thread; the interfaces are used on it only.
        let folder = unsafe {
            let service: ITaskService =
                CoCreateInstance(&TaskScheduler, None, CLSCTX_INPROC_SERVER)
                    .context("creating the Task Scheduler service object")?;
            let empty = VARIANT::default();
            service
                .Connect(&empty, &empty, &empty, &empty)
                .context("connecting to Task Scheduler")?;
            service
                .GetFolder(&BSTR::from("\\"))
                .context("opening the root task folder")?
        };
        f(&folder)
    }

    /// The task, or `None` when it does not exist.
    fn get(folder: &ITaskFolder, name: &str) -> anyhow::Result<Option<IRegisteredTask>> {
        // SAFETY: `folder` is a live interface of this thread.
        match unsafe { folder.GetTask(&BSTR::from(name)) } {
            Ok(task) => Ok(Some(task)),
            Err(e) if e.code() == TASK_NOT_FOUND => Ok(None),
            Err(e) => Err(e).with_context(|| format!("reading the task {name}")),
        }
    }

    pub fn register(name: &str, xml: &str) -> anyhow::Result<()> {
        with_root(|folder| {
            let empty = VARIANT::default();
            // SAFETY: as above; the credentials come from the definition's principal.
            unsafe {
                folder.RegisterTask(
                    &BSTR::from(name),
                    &BSTR::from(xml),
                    TASK_CREATE_OR_UPDATE.0,
                    &empty,
                    &empty,
                    TASK_LOGON_INTERACTIVE_TOKEN,
                    &empty,
                )
            }
            .with_context(|| format!("registering the task {name}"))?;
            Ok(())
        })
    }

    pub fn query(name: &str) -> anyhow::Result<Option<TaskStatus>> {
        with_root(|folder| {
            let Some(task) = get(folder, name)? else {
                return Ok(None);
            };
            // SAFETY: `task` is a live interface of this thread.
            unsafe {
                let state = match task.State()? {
                    TASK_STATE_DISABLED => "disabled",
                    TASK_STATE_QUEUED => "queued",
                    TASK_STATE_READY => "ready",
                    TASK_STATE_RUNNING => "running",
                    _ => "unknown",
                };
                let last_result = task.LastTaskResult()?;
                // A task that has not run says so with `SCHED_S_TASK_HAS_NOT_RUN` (its last
                // run time is then a placeholder date); no next run is the zero date.
                let last_run =
                    (last_result != super::SCHED_S_TASK_HAS_NOT_RUN).then_some(task.LastRunTime()?);
                let next_run = Some(task.NextRunTime()?).filter(|d| *d > 0.0);
                Ok(Some(TaskStatus {
                    name: name.to_owned(),
                    enabled: task.Enabled()? != VARIANT_BOOL(0),
                    state,
                    last_run,
                    last_result,
                    next_run,
                }))
            }
        })
    }

    pub fn delete(name: &str) -> anyhow::Result<bool> {
        with_root(|folder| {
            // SAFETY: as above.
            match unsafe { folder.DeleteTask(&BSTR::from(name), 0) } {
                Ok(()) => Ok(true),
                Err(e) if e.code() == TASK_NOT_FOUND => Ok(false),
                Err(e) => Err(e).with_context(|| format!("deleting the task {name}")),
            }
        })
    }

    pub fn run(name: &str) -> anyhow::Result<()> {
        with_root(|folder| {
            let task =
                get(folder, name)?.with_context(|| format!("the task {name} does not exist"))?;
            // SAFETY: as above; no parameters.
            unsafe { task.Run(&VARIANT::default()) }
                .with_context(|| format!("starting the task {name}"))?;
            Ok(())
        })
    }
}

#[cfg(not(windows))]
mod scheduler {
    use super::TaskStatus;

    const UNSUPPORTED: &str = "Task Scheduler exists on Windows only";

    pub fn register(_name: &str, _xml: &str) -> anyhow::Result<()> {
        anyhow::bail!(UNSUPPORTED)
    }
    pub fn query(_name: &str) -> anyhow::Result<Option<TaskStatus>> {
        anyhow::bail!(UNSUPPORTED)
    }
    pub fn delete(_name: &str) -> anyhow::Result<bool> {
        anyhow::bail!(UNSUPPORTED)
    }
    pub fn run(_name: &str) -> anyhow::Result<()> {
        anyhow::bail!(UNSUPPORTED)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths() -> Paths {
        Paths {
            config_dir: PathBuf::from(r"C:\Users\me\AppData\Roaming\agent-app-server"),
            data_dir: PathBuf::from(r"C:\Users\me\AppData\Local\agent app server"),
        }
    }

    #[test]
    fn the_tasks_pass_the_folders_and_the_keepalive_argument() {
        let [logon, keepalive] = definitions(
            Path::new(r"C:\Apps & Tools\agent-app-server-daemon.exe"),
            &paths(),
            Duration::from_secs(300),
            r"PC\me",
        );
        let xml = task_xml(&logon);
        assert!(
            xml.contains(r"<Command>C:\Apps &amp; Tools\agent-app-server-daemon.exe</Command>")
        );
        assert!(xml.contains(
            r"<Arguments>--config-dir C:\Users\me\AppData\Roaming\agent-app-server --data-dir &quot;C:\Users\me\AppData\Local\agent app server&quot;</Arguments>"
        ));
        assert!(xml.contains("<LogonTrigger>") && !xml.contains("<TimeTrigger>"));
        assert!(xml.contains("<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>"));
        assert!(xml.contains("<MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>"));
        assert!(xml.contains(r"<UserId>PC\me</UserId>"));
        assert!(xml.contains(r"<URI>\agent-app-server</URI>"));
        assert!(!xml.contains("<RestartOnFailure>"));
        let xml = task_xml(&keepalive);
        assert!(xml.contains("<Arguments>--keepalive --config-dir"));
        assert!(xml.contains("<Interval>PT5M</Interval>") && !xml.contains("<LogonTrigger>"));
        assert!(xml.contains(r"<URI>\agent-app-server-keepalive</URI>"));
    }

    /// Read-only against the real Task Scheduler: "does not exist" is an answer, not an error.
    #[cfg(windows)]
    #[tokio::test]
    async fn a_task_that_does_not_exist_is_reported_missing_not_as_an_error() {
        let name = format!("aas-test-never-registered-{}", std::process::id());
        assert_eq!(query(&name).await.unwrap(), None);
        assert!(!delete(&name).await.unwrap());
        let err = run(&name).await.unwrap_err();
        assert!(format!("{err:#}").contains("does not exist"), "{err:#}");
    }

    #[test]
    fn arguments_are_quoted_for_the_command_line() {
        assert_eq!(quote_arg("plain"), "plain");
        assert_eq!(quote_arg(""), "\"\"");
        assert_eq!(quote_arg(r"C:\a b"), r#""C:\a b""#);
        assert_eq!(quote_arg(r"C:\a b\"), r#""C:\a b\\""#);
        assert_eq!(quote_arg(r#"say "hi""#), r#""say \"hi\"""#);
        assert_eq!(quote_arg(r"no\space"), r"no\space");
    }

    #[test]
    fn durations_dates_and_results_are_written_as_task_scheduler_uses_them() {
        assert_eq!(xs_duration(Duration::from_secs(60)), "PT1M");
        assert_eq!(xs_duration(Duration::from_secs(300)), "PT5M");
        assert_eq!(xs_duration(Duration::from_secs(3661)), "PT1H1M1S");
        assert_eq!(xs_duration(Duration::from_secs(86_400)), "P1D");
        assert_eq!(xs_duration(Duration::from_secs(90_000)), "P1DT1H");
        // 2026-09-28 12:30:00 is 46293.520833… days after 1899-12-30.
        assert_eq!(format_ole_date(46_293.520_833_333), "2026-09-28 12:30:00");
        assert_eq!(format_ole_date(2.0), "1900-01-01 00:00:00");
        assert_eq!(describe_result(0), "exit code 0");
        assert_eq!(describe_result(2), "exit code 2");
        assert_eq!(
            describe_result(SCHED_S_TASK_HAS_NOT_RUN),
            "none (has not run)"
        );
        assert_eq!(describe_result(0x8007_0002_u32 as i32), "0x80070002");
    }
}
