//! The active Windows power plan's idle sleep settings, read through the Power API
//! (`PowerGetActiveScheme`, `PowerReadACValueIndex`, `PowerReadDCValueIndex`) — not by parsing
//! `powercfg` output. `doctor` warns when the PC will sleep while idle: a sleeping PC is
//! unreachable from the phone.
//!
//! Also the kind of sleep the machine has (`CallNtPowerInformation(SystemPowerCapabilities)`),
//! which decides what the daemon's keep-awake (`SetThreadExecutionState(ES_SYSTEM_REQUIRED)`)
//! can prevent: it only ever holds off *idle* sleep. Windows ends such requests when the user
//! puts the PC to sleep (lid, power button, Start > Sleep), and on a Modern Standby PC it also
//! ends them after five minutes on battery ("Power requests are terminated after 5 minutes on
//! Modern Standby systems on DC power", POWER_REQUEST_TYPE in the WDK documentation).

use crate::config::KeepAwake;
use crate::doctor::Status;

/// Idle timeouts of the active plan, in seconds (0 = never).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdleTimeouts {
    pub ac_sleep: u32,
    pub dc_sleep: u32,
    pub ac_hibernate: u32,
    pub dc_hibernate: u32,
    /// Whether the PC has a system battery (only then do the DC values apply).
    pub has_battery: bool,
}

fn describe(seconds: u32) -> String {
    match seconds {
        0 => "never".into(),
        s if s % 3600 == 0 => format!("after {} h", s / 3600),
        s if s % 60 == 0 => format!("after {} min", s / 60),
        s => format!("after {s} s"),
    }
}

/// The earliest of a sleep and a hibernate timeout (0 = never).
fn earliest(sleep: u32, hibernate: u32) -> u32 {
    match (sleep, hibernate) {
        (0, h) => h,
        (s, 0) => s,
        (s, h) => s.min(h),
    }
}

/// `doctor`'s verdict on the power plan.
pub fn evaluate(t: &IdleTimeouts, keep_awake: KeepAwake) -> (Status, String) {
    let ac = earliest(t.ac_sleep, t.ac_hibernate);
    let dc = earliest(t.dc_sleep, t.dc_hibernate);
    let plan = if t.has_battery {
        format!(
            "idle sleep {} when plugged in, {} on battery",
            describe(ac),
            describe(dc)
        )
    } else {
        format!("idle sleep {}", describe(ac))
    };
    if keep_awake == KeepAwake::Always {
        return (
            Status::Ok,
            format!(
                "{plan}; the daemon keeps the PC awake while it runs (power.keep_awake = \"always\")"
            ),
        );
    }
    let sleeps = ac != 0 || (t.has_battery && dc != 0);
    if sleeps {
        (
            Status::Warn,
            format!(
                "{plan}: while asleep the PC cannot be reached from the phone (the daemon only keeps it awake while turns run). \
                 Set sleep to \"Never\" (Settings > System > Power), or set [power] keep_awake = \"always\""
            ),
        )
    } else {
        (
            Status::Ok,
            format!("{plan}: the PC stays reachable while idle"),
        )
    }
}

/// The kind of sleep of this machine (`SYSTEM_POWER_CAPABILITIES`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SleepCapabilities {
    /// Modern Standby (`AoAc`: S0 low-power idle): "sleep" is a standby in which the display
    /// is off, desktop programs such as the daemon are paused and the network may be off.
    pub modern_standby: bool,
    /// Traditional sleep (S3).
    pub s3: bool,
    pub lid: bool,
    pub battery: bool,
}

/// `doctor`'s explanation of what keep-awake can and cannot prevent on this machine.
pub fn explain_keep_awake(c: &SleepCapabilities, keep_awake: KeepAwake) -> (Status, String) {
    let held = match keep_awake {
        KeepAwake::Always => "as long as the daemon runs",
        KeepAwake::WhileRunning => "while a turn runs",
    };
    let user_sleep = if c.lid {
        "closing the lid, the power button or Start > Sleep"
    } else {
        "the power button or Start > Sleep"
    };
    let kind = if c.modern_standby {
        "Modern Standby"
    } else if c.s3 {
        "traditional sleep (S3)"
    } else {
        "no sleep state"
    };
    if !c.modern_standby && !c.s3 {
        return (
            Status::Ok,
            format!("{kind}: this PC does not sleep, so the daemon stays reachable"),
        );
    }
    let what_it_prevents = format!(
        "{kind}: keep-awake holds off idle sleep {held}, but never {user_sleep} — those put the PC to sleep and make it unreachable from the phone"
    );
    if c.modern_standby && c.battery {
        (
            Status::Warn,
            format!(
                "{what_it_prevents}. On battery Windows also ends the keep-awake request after 5 minutes, so the PC can go into standby during a turn: keep it plugged in"
            ),
        )
    } else {
        (Status::Ok, what_it_prevents)
    }
}

/// Reads the machine's sleep capabilities.
#[cfg(windows)]
pub fn read_capabilities() -> Result<SleepCapabilities, String> {
    use windows::Win32::System::Power::{
        CallNtPowerInformation, SYSTEM_POWER_CAPABILITIES, SystemPowerCapabilities,
    };
    let mut caps = SYSTEM_POWER_CAPABILITIES::default();
    // SAFETY: the output buffer is a live SYSTEM_POWER_CAPABILITIES of the size passed.
    let status = unsafe {
        CallNtPowerInformation(
            SystemPowerCapabilities,
            None,
            0,
            Some((&mut caps as *mut SYSTEM_POWER_CAPABILITIES).cast()),
            std::mem::size_of::<SYSTEM_POWER_CAPABILITIES>() as u32,
        )
    };
    if status.is_err() {
        return Err(format!(
            "CallNtPowerInformation(SystemPowerCapabilities) failed: NTSTATUS 0x{:08X}",
            status.0 as u32
        ));
    }
    Ok(SleepCapabilities {
        modern_standby: caps.AoAc,
        s3: caps.SystemS3,
        lid: caps.LidPresent,
        battery: caps.SystemBatteriesPresent,
    })
}

#[cfg(not(windows))]
pub fn read_capabilities() -> Result<SleepCapabilities, String> {
    Err("power capabilities are read on Windows only".into())
}

/// Reads the active plan.
#[cfg(windows)]
pub fn read() -> Result<IdleTimeouts, String> {
    use windows::Win32::Foundation::{HLOCAL, LocalFree};
    use windows::Win32::System::Power::{
        GetSystemPowerStatus, PowerGetActiveScheme, PowerReadACValueIndex, PowerReadDCValueIndex,
        SYSTEM_POWER_STATUS,
    };
    use windows::core::GUID;

    // Sleep subgroup and its "Sleep after" / "Hibernate after" settings (winnt.h).
    const GUID_SLEEP_SUBGROUP: GUID = GUID::from_u128(0x238c9fa8_0aad_41ed_83f4_97be242c8f20);
    const GUID_STANDBY_TIMEOUT: GUID = GUID::from_u128(0x29f6c1db_86da_48c5_9fdb_f2b67b1f44da);
    const GUID_HIBERNATE_TIMEOUT: GUID = GUID::from_u128(0x9d7815a6_7ee4_497e_8888_515a05f02364);
    /// `SYSTEM_POWER_STATUS.BatteryFlag`: no system battery.
    const NO_SYSTEM_BATTERY: u8 = 128;
    /// `SYSTEM_POWER_STATUS.BatteryFlag`: unable to read the battery flag information.
    const BATTERY_UNKNOWN: u8 = 255;

    let mut scheme: *mut GUID = std::ptr::null_mut();
    // SAFETY: PowerGetActiveScheme allocates the GUID with LocalAlloc; it is freed below.
    let status = unsafe { PowerGetActiveScheme(None, &mut scheme) };
    if status.is_err() || scheme.is_null() {
        return Err(format!(
            "PowerGetActiveScheme failed: {}",
            windows::core::Error::from(status.to_hresult())
        ));
    }
    let read = |setting: &GUID, ac: bool| -> Result<u32, String> {
        let mut value = 0u32;
        let code = if ac {
            // SAFETY: `scheme` is a valid GUID returned above; the other pointers are live.
            unsafe {
                PowerReadACValueIndex(
                    None,
                    Some(scheme),
                    Some(&GUID_SLEEP_SUBGROUP),
                    Some(setting),
                    &mut value,
                )
            }
            .0
        } else {
            // SAFETY: as above.
            unsafe {
                PowerReadDCValueIndex(
                    None,
                    Some(scheme),
                    Some(&GUID_SLEEP_SUBGROUP),
                    Some(setting),
                    &mut value,
                )
            }
        };
        if code != 0 {
            return Err(format!("reading the power plan failed (error {code})"));
        }
        Ok(value)
    };
    let result: Result<IdleTimeouts, String> = (|| {
        Ok(IdleTimeouts {
            ac_sleep: read(&GUID_STANDBY_TIMEOUT, true)?,
            dc_sleep: read(&GUID_STANDBY_TIMEOUT, false)?,
            ac_hibernate: read(&GUID_HIBERNATE_TIMEOUT, true)?,
            dc_hibernate: read(&GUID_HIBERNATE_TIMEOUT, false)?,
            has_battery: false,
        })
    })();
    // SAFETY: `scheme` was allocated by PowerGetActiveScheme with LocalAlloc and is not used
    // after this.
    unsafe { LocalFree(Some(HLOCAL(scheme.cast()))) };
    let mut timeouts = result?;
    let mut power = SYSTEM_POWER_STATUS::default();
    // SAFETY: `power` is a live out-parameter.
    unsafe { GetSystemPowerStatus(&mut power) }
        .map_err(|e| format!("GetSystemPowerStatus failed: {e}"))?;
    timeouts.has_battery =
        power.BatteryFlag != NO_SYSTEM_BATTERY && power.BatteryFlag != BATTERY_UNKNOWN;
    Ok(timeouts)
}

#[cfg(not(windows))]
pub fn read() -> Result<IdleTimeouts, String> {
    Err("power plans are a Windows feature".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(ac_sleep: u32, dc_sleep: u32, has_battery: bool) -> IdleTimeouts {
        IdleTimeouts {
            ac_sleep,
            dc_sleep,
            ac_hibernate: 0,
            dc_hibernate: 0,
            has_battery,
        }
    }

    #[test]
    fn a_pc_that_sleeps_while_idle_is_a_warning() {
        let (status, detail) = evaluate(&t(1800, 0, false), KeepAwake::WhileRunning);
        assert_eq!(status, Status::Warn);
        assert!(
            detail.contains("after 30 min") && detail.contains("keep_awake"),
            "{detail}"
        );
        assert_eq!(
            evaluate(&t(0, 0, false), KeepAwake::WhileRunning).0,
            Status::Ok
        );
        // Battery settings only matter with a battery.
        assert_eq!(
            evaluate(&t(0, 600, false), KeepAwake::WhileRunning).0,
            Status::Ok
        );
        assert_eq!(
            evaluate(&t(0, 600, true), KeepAwake::WhileRunning).0,
            Status::Warn
        );
        // Hibernation counts as sleep.
        let hibernates = IdleTimeouts {
            ac_hibernate: 7200,
            ..t(0, 0, false)
        };
        assert!(
            evaluate(&hibernates, KeepAwake::WhileRunning)
                .1
                .contains("after 2 h")
        );
        // Keeping the PC awake always makes the plan irrelevant while the daemon runs.
        assert_eq!(
            evaluate(&t(1800, 600, true), KeepAwake::Always).0,
            Status::Ok
        );
    }

    fn caps(modern_standby: bool, s3: bool, lid: bool, battery: bool) -> SleepCapabilities {
        SleepCapabilities {
            modern_standby,
            s3,
            lid,
            battery,
        }
    }

    #[test]
    fn keep_awake_is_explained_for_the_kind_of_sleep_of_the_machine() {
        // A Modern Standby laptop: on battery Windows ends the request after 5 minutes.
        let (status, detail) =
            explain_keep_awake(&caps(true, false, true, true), KeepAwake::WhileRunning);
        assert_eq!(status, Status::Warn);
        assert!(detail.contains("Modern Standby"), "{detail}");
        assert!(detail.contains("5 minutes"), "{detail}");
        assert!(detail.contains("closing the lid"), "{detail}");
        assert!(detail.contains("while a turn runs"), "{detail}");
        // A Modern Standby desktop: idle standby is held off; the power button is not.
        let (status, detail) =
            explain_keep_awake(&caps(true, false, false, false), KeepAwake::Always);
        assert_eq!(status, Status::Ok);
        assert!(detail.contains("as long as the daemon runs"), "{detail}");
        assert!(
            detail.contains("the power button") && !detail.contains("lid"),
            "{detail}"
        );
        assert!(!detail.contains("5 minutes"), "{detail}");
        // Traditional sleep: the battery does not end the request.
        let (status, detail) =
            explain_keep_awake(&caps(false, true, true, true), KeepAwake::WhileRunning);
        assert_eq!(status, Status::Ok);
        assert!(
            detail.contains("S3") && detail.contains("closing the lid"),
            "{detail}"
        );
        // No sleep state at all.
        let (status, detail) =
            explain_keep_awake(&caps(false, false, false, false), KeepAwake::WhileRunning);
        assert_eq!(status, Status::Ok);
        assert!(detail.contains("does not sleep"), "{detail}");
    }

    #[cfg(windows)]
    #[test]
    fn the_sleep_capabilities_can_be_read() {
        let c = read_capabilities().unwrap();
        // Whatever the machine, the explanation names its kind of sleep.
        let (_, detail) = explain_keep_awake(&c, KeepAwake::WhileRunning);
        assert!(
            detail.contains("Modern Standby")
                || detail.contains("S3")
                || detail.contains("does not sleep"),
            "{c:?}: {detail}"
        );
    }

    #[cfg(windows)]
    #[test]
    fn the_active_plan_can_be_read() {
        let timeouts = read().unwrap();
        // Values are seconds; a plan never uses more than a few days.
        assert!(timeouts.ac_sleep < 30 * 24 * 3600, "{timeouts:?}");
    }
}
