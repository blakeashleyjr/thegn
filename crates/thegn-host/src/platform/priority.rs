//! Identity-bound process priority operations.
//!
//! Linux `setpriority` and `ioprio_set` have no pidfd form. The identity checks
//! before and after each syscall therefore leave a small check-act-check race;
//! a post-call mismatch is reported as [`PriorityError::Changed`] and logged.

use super::ProcStart;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IoClass {
    RealTime(u8),
    BestEffort(u8),
    Idle,
    None,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PriorityError {
    Changed,
    Denied(String),
    Unsupported,
    Failed(String),
}

#[cfg(target_os = "linux")]
pub fn get_nice(pid: u32, start: ProcStart) -> Result<i32, PriorityError> {
    let before = linux_identity_matches(pid, start);
    if !before {
        return Err(PriorityError::Changed);
    }
    // SAFETY: errno is thread-local and getpriority has no pointer arguments.
    unsafe { *libc::__errno_location() = 0 };
    // SAFETY: pid is range-checked by linux_identity_matches; PRIO_PROCESS uses
    // the id argument as a process id.
    let result = unsafe { libc::getpriority(libc::PRIO_PROCESS, pid) };
    // SAFETY: read this thread's errno immediately after getpriority.
    let errno = unsafe { *libc::__errno_location() };
    let result = if result == -1 && errno != 0 {
        Err(map_linux_error(std::io::Error::from_raw_os_error(errno)))
    } else {
        Ok(result)
    };
    check_linux_identity_after(pid, start)?;
    result
}

#[cfg(target_os = "linux")]
pub fn set_nice(pid: u32, start: ProcStart, nice: i32) -> Result<(), PriorityError> {
    if !linux_identity_matches(pid, start) {
        return Err(PriorityError::Changed);
    }
    // SAFETY: pid is range-checked above; setpriority has no pointer arguments.
    let result = unsafe { libc::setpriority(libc::PRIO_PROCESS, pid, nice.clamp(-20, 19)) };
    let call_result = if result == 0 {
        Ok(())
    } else {
        Err(map_linux_error(std::io::Error::last_os_error()))
    };
    check_linux_identity_after(pid, start)?;
    call_result
}

#[cfg(target_os = "linux")]
pub fn get_ioprio(pid: u32, start: ProcStart) -> Result<IoClass, PriorityError> {
    if !linux_identity_matches(pid, start) {
        return Err(PriorityError::Changed);
    }
    // SAFETY: syscall arguments follow ioprio_get(2); pid is range-checked.
    let result = unsafe {
        libc::syscall(
            libc::SYS_ioprio_get,
            1, // IOPRIO_WHO_PROCESS
            pid as libc::pid_t,
        )
    };
    let call_result = if result < 0 {
        Err(map_linux_error(std::io::Error::last_os_error()))
    } else {
        decode_ioprio(result as u32)
    };
    check_linux_identity_after(pid, start)?;
    call_result
}

#[cfg(target_os = "linux")]
pub fn set_ioprio(pid: u32, start: ProcStart, class: IoClass) -> Result<(), PriorityError> {
    if !linux_identity_matches(pid, start) {
        return Err(PriorityError::Changed);
    }
    let encoded = encode_ioprio(class);
    // SAFETY: syscall arguments follow ioprio_set(2); pid is range-checked.
    let result = unsafe {
        libc::syscall(
            libc::SYS_ioprio_set,
            1, // IOPRIO_WHO_PROCESS
            pid as libc::pid_t,
            encoded,
        )
    };
    let call_result = if result == 0 {
        Ok(())
    } else {
        Err(map_linux_error(std::io::Error::last_os_error()))
    };
    check_linux_identity_after(pid, start)?;
    call_result
}

#[cfg(target_os = "linux")]
fn linux_identity_matches(pid: u32, start: ProcStart) -> bool {
    if pid == 0 || pid > i32::MAX as u32 {
        return false;
    }
    let live_ticks = super::signal_identity::read_start_ticks(pid);
    super::signal_identity::identity_matches(start, live_ticks, || None)
}

#[cfg(target_os = "linux")]
fn check_linux_identity_after(pid: u32, start: ProcStart) -> Result<(), PriorityError> {
    if linux_identity_matches(pid, start) {
        Ok(())
    } else {
        tracing::warn!(pid, "process identity changed during priority operation");
        Err(PriorityError::Changed)
    }
}

#[cfg(target_os = "linux")]
fn map_linux_error(error: std::io::Error) -> PriorityError {
    match error.raw_os_error() {
        Some(libc::EPERM | libc::EACCES) => PriorityError::Denied(error.to_string()),
        _ => PriorityError::Failed(error.to_string()),
    }
}

#[cfg(target_os = "linux")]
fn encode_ioprio(class: IoClass) -> libc::c_int {
    let (class, data) = match class {
        IoClass::RealTime(data) => (1, data),
        IoClass::BestEffort(data) => (2, data),
        IoClass::Idle => (3, 0),
        IoClass::None => (0, 0),
    };
    (class << 13) | i32::from(data)
}

#[cfg(target_os = "linux")]
fn decode_ioprio(value: u32) -> Result<IoClass, PriorityError> {
    let class = (value >> 13) & 0x7;
    let data = (value & 0x1fff) as u8;
    match class {
        0 => Ok(IoClass::None),
        1 => Ok(IoClass::RealTime(data)),
        2 => Ok(IoClass::BestEffort(data)),
        3 => Ok(IoClass::Idle),
        _ => Err(PriorityError::Failed("unknown I/O priority class".into())),
    }
}

#[cfg(target_os = "macos")]
pub fn get_nice(pid: u32, start: ProcStart) -> Result<i32, PriorityError> {
    if !mac_identity_matches(pid, start) {
        return Err(PriorityError::Changed);
    }
    // SAFETY: errno is thread-local and getpriority has no pointer arguments.
    unsafe { *libc::__error() = 0 };
    // SAFETY: pid is range-checked by mac_identity_matches.
    let result = unsafe { libc::getpriority(libc::PRIO_PROCESS, pid) };
    // SAFETY: read errno immediately after getpriority.
    let errno = unsafe { *libc::__error() };
    let call_result = if result == -1 && errno != 0 {
        Err(map_unix_error(std::io::Error::from_raw_os_error(errno)))
    } else {
        Ok(result)
    };
    check_mac_identity_after(pid, start)?;
    call_result
}

#[cfg(target_os = "macos")]
pub fn set_nice(pid: u32, start: ProcStart, nice: i32) -> Result<(), PriorityError> {
    if !mac_identity_matches(pid, start) {
        return Err(PriorityError::Changed);
    }
    // SAFETY: pid is range-checked above; setpriority has no pointer arguments.
    let result = unsafe { libc::setpriority(libc::PRIO_PROCESS, pid, nice.clamp(-20, 19)) };
    let call_result = if result == 0 {
        Ok(())
    } else {
        Err(map_unix_error(std::io::Error::last_os_error()))
    };
    check_mac_identity_after(pid, start)?;
    call_result
}

#[cfg(target_os = "macos")]
fn mac_identity_matches(pid: u32, start: ProcStart) -> bool {
    if pid == 0 || pid > i32::MAX as u32 {
        return false;
    }
    // SAFETY: zeroed POD out-param; proc_pidinfo writes at most its size.
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let want = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // SAFETY: valid writable buffer and exact buffer length.
    let got = unsafe {
        libc::proc_pidinfo(
            pid as libc::pid_t,
            libc::PROC_PIDTBSDINFO,
            0,
            std::ptr::from_mut(&mut info).cast(),
            want,
        )
    };
    got == want && info.pbi_start_tvsec == start.secs
}

#[cfg(target_os = "macos")]
fn check_mac_identity_after(pid: u32, start: ProcStart) -> Result<(), PriorityError> {
    if mac_identity_matches(pid, start) {
        Ok(())
    } else {
        tracing::warn!(pid, "process identity changed during priority operation");
        Err(PriorityError::Changed)
    }
}

#[cfg(target_os = "macos")]
fn map_unix_error(error: std::io::Error) -> PriorityError {
    match error.raw_os_error() {
        Some(libc::EPERM | libc::EACCES) => PriorityError::Denied(error.to_string()),
        _ => PriorityError::Failed(error.to_string()),
    }
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
pub fn get_nice(_pid: u32, _start: ProcStart) -> Result<i32, PriorityError> {
    Err(PriorityError::Unsupported)
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
pub fn set_nice(_pid: u32, _start: ProcStart, _nice: i32) -> Result<(), PriorityError> {
    Err(PriorityError::Unsupported)
}

#[cfg(windows)]
pub fn get_nice(pid: u32, start: ProcStart) -> Result<i32, PriorityError> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        ABOVE_NORMAL_PRIORITY_CLASS, BELOW_NORMAL_PRIORITY_CLASS, GetPriorityClass,
        HIGH_PRIORITY_CLASS, IDLE_PRIORITY_CLASS, NORMAL_PRIORITY_CLASS,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };
    // SAFETY: successful OpenProcess handle is closed on every path.
    unsafe {
        let handle = open_priority_process(pid, PROCESS_QUERY_LIMITED_INFORMATION)?;
        let identity = windows_identity_matches(handle, start);
        let priority = GetPriorityClass(handle);
        let error = std::io::Error::last_os_error();
        let changed = !windows_identity_matches(handle, start);
        CloseHandle(handle);
        if !identity || changed {
            return Err(PriorityError::Changed);
        }
        if priority == 0 {
            return Err(map_windows_error(error));
        }
        match priority {
            IDLE_PRIORITY_CLASS => Ok(19),
            BELOW_NORMAL_PRIORITY_CLASS => Ok(10),
            NORMAL_PRIORITY_CLASS => Ok(0),
            ABOVE_NORMAL_PRIORITY_CLASS => Ok(-10),
            HIGH_PRIORITY_CLASS => Ok(-20),
            _ => Err(PriorityError::Failed(
                "unknown process priority class".into(),
            )),
        }
    }
}

#[cfg(windows)]
pub fn set_nice(pid: u32, start: ProcStart, nice: i32) -> Result<(), PriorityError> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        ABOVE_NORMAL_PRIORITY_CLASS, BELOW_NORMAL_PRIORITY_CLASS, HIGH_PRIORITY_CLASS,
        IDLE_PRIORITY_CLASS, NORMAL_PRIORITY_CLASS, PROCESS_QUERY_LIMITED_INFORMATION,
        PROCESS_SET_INFORMATION, SetPriorityClass,
    };
    let nice = nice.clamp(-20, 19);
    let class = match nice {
        -20..=-11 => HIGH_PRIORITY_CLASS,
        -10..=-1 => ABOVE_NORMAL_PRIORITY_CLASS,
        0..=9 => NORMAL_PRIORITY_CLASS,
        10..=18 => BELOW_NORMAL_PRIORITY_CLASS,
        _ => IDLE_PRIORITY_CLASS,
    };
    // SAFETY: successful OpenProcess handle is closed on every path.
    unsafe {
        let handle = open_priority_process(
            pid,
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SET_INFORMATION,
        )?;
        if !windows_identity_matches(handle, start) {
            CloseHandle(handle);
            return Err(PriorityError::Changed);
        }
        let success = SetPriorityClass(handle, class) != 0;
        let error = std::io::Error::last_os_error();
        let changed = !windows_identity_matches(handle, start);
        CloseHandle(handle);
        if changed {
            return Err(PriorityError::Changed);
        }
        if success {
            Ok(())
        } else {
            Err(map_windows_error(error))
        }
    }
}

#[cfg(windows)]
unsafe fn open_priority_process(
    pid: u32,
    access: u32,
) -> Result<windows_sys::Win32::Foundation::HANDLE, PriorityError> {
    use windows_sys::Win32::System::Threading::OpenProcess;
    if pid == 0 || pid > i32::MAX as u32 {
        return Err(PriorityError::Changed);
    }
    // SAFETY: caller owns and closes the returned process handle.
    let handle = unsafe { OpenProcess(access, 0, pid) };
    if handle.is_null() {
        Err(map_windows_error(std::io::Error::last_os_error()))
    } else {
        Ok(handle)
    }
}

#[cfg(windows)]
unsafe fn windows_identity_matches(
    handle: windows_sys::Win32::Foundation::HANDLE,
    start: ProcStart,
) -> bool {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::GetProcessTimes;
    let mut creation = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let mut exit = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let mut kernel = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let mut user = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    // SAFETY: pointers refer to writable FILETIME values for the call duration.
    if unsafe { GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) } == 0 {
        return false;
    }
    let ticks = (u64::from(creation.dwHighDateTime) << 32) | u64::from(creation.dwLowDateTime);
    ticks
        .saturating_div(10_000_000)
        .saturating_sub(11_644_473_600)
        == start.secs
}

#[cfg(windows)]
fn map_windows_error(error: std::io::Error) -> PriorityError {
    match error.raw_os_error() {
        Some(5) => PriorityError::Denied(error.to_string()),
        _ => PriorityError::Failed(error.to_string()),
    }
}

#[cfg(any(windows, all(unix, not(target_os = "linux"))))]
pub fn get_ioprio(_pid: u32, _start: ProcStart) -> Result<IoClass, PriorityError> {
    Err(PriorityError::Unsupported)
}

#[cfg(any(windows, all(unix, not(target_os = "linux"))))]
pub fn set_ioprio(_pid: u32, _start: ProcStart, _class: IoClass) -> Result<(), PriorityError> {
    Err(PriorityError::Unsupported)
}

#[cfg(not(any(unix, windows)))]
pub fn get_nice(_pid: u32, _start: ProcStart) -> Result<i32, PriorityError> {
    Err(PriorityError::Unsupported)
}

#[cfg(not(any(unix, windows)))]
pub fn set_nice(_pid: u32, _start: ProcStart, _nice: i32) -> Result<(), PriorityError> {
    Err(PriorityError::Unsupported)
}

#[cfg(not(any(unix, windows)))]
pub fn get_ioprio(_pid: u32, _start: ProcStart) -> Result<IoClass, PriorityError> {
    Err(PriorityError::Unsupported)
}

#[cfg(not(any(unix, windows)))]
pub fn set_ioprio(_pid: u32, _start: ProcStart, _class: IoClass) -> Result<(), PriorityError> {
    Err(PriorityError::Unsupported)
}

#[cfg(all(test, target_os = "linux"))]
#[expect(
    clippy::disallowed_methods,
    reason = "tests own and reap their sleep child processes"
)]
mod tests {
    use super::{IoClass, PriorityError, get_ioprio, get_nice, set_ioprio, set_nice};
    use crate::platform::ProcStart;
    use std::process::{Child, Command};

    struct ChildGuard(Child);

    impl Drop for ChildGuard {
        fn drop(&mut self) {
            match self.0.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) => {}
                Err(error) => {
                    tracing::debug!(%error, "could not check fixture child before cleanup")
                }
            }
            if let Err(error) = self.0.kill() {
                tracing::debug!(%error, "could not kill fixture child during cleanup");
            }
            if let Err(error) = self.0.wait() {
                tracing::debug!(%error, "could not reap fixture child during cleanup");
            }
        }
    }

    fn spawn_sleep() -> ChildGuard {
        ChildGuard(
            Command::new("sleep")
                .arg("30")
                .spawn()
                .expect("spawn sleep child"),
        )
    }

    fn child_start(child: &ChildGuard) -> ProcStart {
        ProcStart {
            secs: 0,
            ticks: super::super::signal_identity::read_start_ticks(child.0.id()),
        }
    }

    #[test]
    fn own_child_nice_and_ioprio_round_trip() {
        let child = spawn_sleep();
        let start = child_start(&child);
        set_nice(child.0.id(), start, 5).expect("raise child nice to 5");
        assert_eq!(get_nice(child.0.id(), start), Ok(5));
        set_nice(child.0.id(), start, 10).expect("raise child nice to 10");
        assert_eq!(get_nice(child.0.id(), start), Ok(10));
        set_ioprio(child.0.id(), start, IoClass::BestEffort(7))
            .expect("set child best-effort I/O priority");
        assert_eq!(get_ioprio(child.0.id(), start), Ok(IoClass::BestEffort(7)));
        set_ioprio(child.0.id(), start, IoClass::Idle).expect("set child idle I/O priority");
        assert_eq!(get_ioprio(child.0.id(), start), Ok(IoClass::Idle));
    }

    #[test]
    fn wrong_start_refuses_renice_without_changing_the_child() {
        let child = spawn_sleep();
        let start = child_start(&child);
        let original = get_nice(child.0.id(), start).expect("read child nice");
        let wrong = ProcStart {
            secs: start.secs,
            ticks: start.ticks.map(|ticks| ticks + 1),
        };
        assert_eq!(
            set_nice(child.0.id(), wrong, 5),
            Err(PriorityError::Changed)
        );
        assert_eq!(get_nice(child.0.id(), start), Ok(original));
    }

    #[test]
    fn exited_pid_is_rejected_after_child_is_reaped() {
        let mut child = spawn_sleep();
        let start = child_start(&child);
        child.0.kill().expect("stop child");
        child.0.wait().expect("reap child");
        assert!(matches!(
            get_nice(child.0.id(), start),
            Err(PriorityError::Changed | PriorityError::Failed(_))
        ));
    }

    #[test]
    fn nice_value_is_clamped_at_nineteen() {
        let child = spawn_sleep();
        let start = child_start(&child);
        set_nice(child.0.id(), start, 40).expect("raise child nice with clamping");
        assert_eq!(get_nice(child.0.id(), start), Ok(19));
    }
}
