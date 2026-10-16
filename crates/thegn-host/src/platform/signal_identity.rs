//! Signal a process only after binding its current identity to the sampled one.

use super::ProcSignal;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignalError {
    Changed,
    Failed(String),
}

pub fn signal_identity(pid: u32, start_secs: u64, sig: ProcSignal) -> Result<(), SignalError> {
    if pid == 0 || pid > i32::MAX as u32 {
        return Err(SignalError::Failed("invalid pid".into()));
    }
    signal_identity_inner(pid, start_secs, sig)
}

#[cfg(target_os = "linux")]
fn signal_identity_inner(pid: u32, start_secs: u64, sig: ProcSignal) -> Result<(), SignalError> {
    use std::os::fd::FromRawFd;

    // SAFETY: pid is range-checked above; pidfd_open returns a new owned fd.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0) };
    if fd < 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ENOSYS) {
            return compare_then_signal(pid, start_secs, sig);
        }
        return Err(map_unix_error(error));
    }
    // SAFETY: successful pidfd_open returned a fresh fd owned by this scope.
    let pidfd = unsafe { std::os::fd::OwnedFd::from_raw_fd(fd as std::os::fd::RawFd) };
    if read_start_secs(pid) != Some(start_secs) {
        return Err(SignalError::Changed);
    }
    let signal = signal_number(sig);
    // SAFETY: pidfd is live and pins the process; null siginfo and flags 0 are documented.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            std::os::fd::AsRawFd::as_raw_fd(&pidfd),
            signal,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        )
    };
    if rc == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if pidfd_unsupported(error.raw_os_error()) {
        return compare_then_signal(pid, start_secs, sig);
    }
    Err(map_unix_error(error))
}

#[cfg(target_os = "linux")]
fn signal_number(sig: ProcSignal) -> libc::c_int {
    match sig {
        ProcSignal::Terminate => libc::SIGTERM,
        ProcSignal::Kill => libc::SIGKILL,
    }
}

#[cfg(target_os = "linux")]
fn pidfd_unsupported(errno: Option<i32>) -> bool {
    errno == Some(libc::ENOSYS)
}

#[cfg(target_os = "linux")]
fn map_unix_error(error: std::io::Error) -> SignalError {
    match error.raw_os_error() {
        Some(libc::ESRCH) => SignalError::Failed("no such process".into()),
        Some(libc::EPERM) => SignalError::Failed("permission denied".into()),
        _ => SignalError::Failed(error.to_string()),
    }
}

#[cfg(target_os = "linux")]
fn read_start_secs(pid: u32) -> Option<u64> {
    static BOOT_TIME: std::sync::OnceLock<Option<u64>> = std::sync::OnceLock::new();
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let ticks = super::unix::parse_proc_start_time(&stat)?;
    let btime = BOOT_TIME.get_or_init(|| {
        std::fs::read_to_string("/proc/stat")
            .ok()
            .and_then(|contents| parse_btime(&contents))
    })?;
    // SAFETY: sysconf with _SC_CLK_TCK has no pointer arguments or side effects.
    let clk_tck = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    start_secs_from_ticks(ticks, u64::try_from(clk_tck).ok()?, btime)
}

#[cfg(target_os = "linux")]
fn compare_then_signal(pid: u32, start_secs: u64, sig: ProcSignal) -> Result<(), SignalError> {
    if read_start_secs(pid) != Some(start_secs) {
        return Err(SignalError::Changed);
    }
    super::signal_pid(pid, sig).map_err(SignalError::Failed)
}

#[cfg(target_os = "linux")]
fn parse_btime(contents: &str) -> Option<u64> {
    contents.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        (fields.next()? == "btime").then(|| fields.next()?.parse().ok())?
    })
}

#[cfg(target_os = "linux")]
fn start_secs_from_ticks(ticks: u64, clk_tck: u64, btime: u64) -> Option<u64> {
    (clk_tck != 0).then(|| btime.checked_add(ticks / clk_tck))?
}

#[cfg(target_os = "macos")]
fn signal_identity_inner(pid: u32, start_secs: u64, sig: ProcSignal) -> Result<(), SignalError> {
    // SAFETY: zeroed POD out-param; the kernel is told its exact size.
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let want = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // SAFETY: valid writable proc_bsdinfo pointer and exact buffer size.
    let got = unsafe {
        libc::proc_pidinfo(
            pid as libc::pid_t,
            libc::PROC_PIDTBSDINFO,
            0,
            std::ptr::from_mut(&mut info).cast(),
            want,
        )
    };
    if got != want || u64::from(info.pbi_start_tvsec) != start_secs {
        return Err(SignalError::Changed);
    }
    super::signal_pid(pid, sig).map_err(SignalError::Failed)
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
fn signal_identity_inner(_pid: u32, _start_secs: u64, _sig: ProcSignal) -> Result<(), SignalError> {
    Err(SignalError::Failed(
        "process identity unavailable on this platform".into(),
    ))
}

#[cfg(windows)]
fn signal_identity_inner(pid: u32, start_secs: u64, _sig: ProcSignal) -> Result<(), SignalError> {
    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME};
    use windows_sys::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE,
        TerminateProcess,
    };

    // SAFETY: pid is range-checked; successful handles are closed on every path.
    unsafe {
        let handle = OpenProcess(
            PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION,
            0,
            pid,
        );
        if handle.is_null() {
            return Err(SignalError::Failed(
                "permission denied or no such process".into(),
            ));
        }
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
        if GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) == 0 {
            CloseHandle(handle);
            return Err(SignalError::Failed(
                "could not read process creation time".into(),
            ));
        }
        let ticks = (u64::from(creation.dwHighDateTime) << 32) | u64::from(creation.dwLowDateTime);
        if filetime_to_unix_secs(ticks) != start_secs {
            CloseHandle(handle);
            return Err(SignalError::Changed);
        }
        let ok = TerminateProcess(handle, 1) != 0;
        CloseHandle(handle);
        if ok {
            Ok(())
        } else {
            Err(SignalError::Failed("terminate failed".into()))
        }
    }
}

#[cfg(not(any(unix, windows)))]
fn signal_identity_inner(_pid: u32, _start_secs: u64, _sig: ProcSignal) -> Result<(), SignalError> {
    Err(SignalError::Failed(
        "process identity unavailable on this platform".into(),
    ))
}

#[cfg(windows)]
fn filetime_to_unix_secs(filetime: u64) -> u64 {
    filetime
        .saturating_div(10_000_000)
        .saturating_sub(11_644_473_600)
}

#[cfg(test)]
mod pure_tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn btime_is_parsed_from_its_named_line() {
        assert_eq!(
            parse_btime("cpu 1 2\nbtime 1700000000\nother 4"),
            Some(1_700_000_000)
        );
        assert_eq!(parse_btime("cpu 1 2\nother 4"), None);
    }

    #[test]
    fn invalid_pids_are_refused_before_platform_calls() {
        assert_eq!(
            signal_identity(0, 1, ProcSignal::Terminate),
            Err(SignalError::Failed("invalid pid".into()))
        );
        assert_eq!(
            signal_identity(u32::MAX, 1, ProcSignal::Terminate),
            Err(SignalError::Failed("invalid pid".into()))
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn ticks_are_converted_to_unix_seconds() {
        assert_eq!(
            start_secs_from_ticks(12_345, 100, 1_700_000_000),
            Some(1_700_000_123)
        );
        assert_eq!(start_secs_from_ticks(12_345, 0, 1_700_000_000), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn enosys_alone_marks_pidfd_as_unsupported() {
        assert!(pidfd_unsupported(Some(libc::ENOSYS)));
        assert!(!pidfd_unsupported(Some(libc::EPERM)));
        assert!(!pidfd_unsupported(None));
    }

    #[cfg(windows)]
    #[test]
    fn filetime_conversion_uses_sysinfo_unix_seconds() {
        assert_eq!(
            filetime_to_unix_secs(132_223_104_000_000_000),
            1_577_836_800
        );
        assert_eq!(filetime_to_unix_secs(0), 0);
    }
}

#[cfg(all(test, target_os = "linux"))]
#[expect(
    clippy::disallowed_methods,
    reason = "tests must reap the child processes they spawn"
)]
mod linux_tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;
    use std::process::{Child, Command};

    fn spawn_sleep() -> Child {
        Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep child")
    }

    fn sampled_start(pid: u32) -> u64 {
        let mut sampler = thegn_metrics::ProcSampler::new(usize::MAX);
        sampler
            .sample()
            .procs
            .into_iter()
            .find(|row| row.pid == pid)
            .expect("sleep child appears in ProcSampler sample")
            .start_time
    }

    #[test]
    fn sampler_seconds_match_the_signal_identity_reader() {
        let mut child = spawn_sleep();
        let pid = child.id();
        let sampled = sampled_start(pid);
        assert_eq!(read_start_secs(pid), Some(sampled));
        child.kill().expect("kill sleep child");
        child.wait().expect("reap sleep child");
    }

    #[test]
    fn matching_identity_sends_sigterm_to_child() {
        let mut child = spawn_sleep();
        let sampled = sampled_start(child.id());
        assert_eq!(
            signal_identity(child.id(), sampled, ProcSignal::Terminate),
            Ok(())
        );
        let status = child.wait().expect("reap signalled child");
        assert_eq!(status.signal(), Some(libc::SIGTERM));
    }

    #[test]
    fn mismatched_identity_leaves_child_alive() {
        let mut child = spawn_sleep();
        let sampled = sampled_start(child.id());
        assert_eq!(
            signal_identity(child.id(), sampled + 1, ProcSignal::Kill),
            Err(SignalError::Changed)
        );
        assert!(child.try_wait().expect("check child status").is_none());
        child.kill().expect("kill sleep child after mismatch");
        child.wait().expect("reap sleep child after mismatch");
    }
}
