//! Self-process thread samples for the opt-in runtime perf report.
//!
//! Linux reads are bounded to the first `cap` task entries. Every per-thread
//! read is best-effort so a thread exiting mid-scan only loses that field.

const COMM_CAP: usize = 16;
const WCHAN_CAP: usize = 32;
const FD_TARGET_CAP: usize = 48;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ThreadSample {
    pub(crate) tid: u32,
    pub(crate) comm: [u8; COMM_CAP],
    pub(crate) comm_len: u8,
    pub(crate) voluntary: Option<u64>,
    pub(crate) nonvoluntary: Option<u64>,
    pub(crate) utime_ticks: Option<u64>,
    pub(crate) syscall: Option<i64>,
    pub(crate) arg0: Option<u64>,
    pub(crate) wchan: [u8; WCHAN_CAP],
    pub(crate) wchan_len: u8,
    pub(crate) fd_target: [u8; FD_TARGET_CAP],
    pub(crate) fd_target_len: u8,
}

impl ThreadSample {
    pub(crate) fn comm_str(&self) -> &str {
        std::str::from_utf8(&self.comm[..usize::from(self.comm_len)]).unwrap_or("")
    }

    pub(crate) fn wchan_str(&self) -> &str {
        std::str::from_utf8(&self.wchan[..usize::from(self.wchan_len)]).unwrap_or("")
    }

    pub(crate) fn fd_target_str(&self) -> &str {
        std::str::from_utf8(&self.fd_target[..usize::from(self.fd_target_len)]).unwrap_or("")
    }
}

#[cfg(target_os = "linux")]
fn copy_text<const N: usize>(dst: &mut [u8; N], src: &str) -> u8 {
    let mut count = src.len().min(N);
    while !src.is_char_boundary(count) {
        count -= 1;
    }
    dst[..count].copy_from_slice(&src.as_bytes()[..count]);
    count as u8
}

/// Parse voluntary and involuntary context switch counters from proc status.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn parse_status(status: &str) -> Option<(u64, u64)> {
    let mut voluntary = None;
    let mut nonvoluntary = None;
    for line in status.lines() {
        if let Some(value) = line.strip_prefix("voluntary_ctxt_switches:") {
            voluntary = value.trim().parse().ok();
        } else if let Some(value) = line.strip_prefix("nonvoluntary_ctxt_switches:") {
            nonvoluntary = value.trim().parse().ok();
        }
    }
    Some((voluntary?, nonvoluntary?))
}

/// Parse the first syscall argument when the task is blocked in a syscall.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn parse_syscall(raw: &str) -> (Option<i64>, Option<u64>) {
    let mut fields = raw.split_whitespace();
    let Some(number) = fields.next() else {
        return (None, None);
    };
    if number == "running" || number == "-1" {
        return (None, None);
    }
    let syscall = number.parse().ok();
    let arg0 = fields.next().and_then(|arg| {
        let hex = arg.strip_prefix("0x").unwrap_or(arg);
        u64::from_str_radix(hex, 16).ok()
    });
    (syscall, arg0)
}

/// Parse field 14 (`utime`) from `/proc/<tid>/stat`.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn parse_stat_utime(stat: &str) -> Option<u64> {
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().nth(11)?.parse().ok()
}

/// Fill `out` with at most `cap` samples from this process.
#[cfg(target_os = "linux")]
pub(crate) fn sample_self(out: &mut Vec<ThreadSample>, cap: usize) {
    use std::path::PathBuf;

    out.clear();
    let Ok(entries) = std::fs::read_dir("/proc/self/task") else {
        return;
    };
    let mut path = PathBuf::from("/proc/self/task");
    let mut text = String::with_capacity(512);
    for entry in entries.flatten().take(cap) {
        let tid_name = entry.file_name();
        let Some(tid) = tid_name.to_str().and_then(|name| name.parse().ok()) else {
            continue;
        };
        path.push(tid_name);
        let mut sample = ThreadSample {
            tid,
            ..ThreadSample::default()
        };
        if read_task_file(&mut path, "comm", &mut text).is_ok() {
            sample.comm_len = copy_text(&mut sample.comm, text.trim_end());
        }
        if read_task_file(&mut path, "status", &mut text).is_ok()
            && let Some((voluntary, nonvoluntary)) = parse_status(&text)
        {
            sample.voluntary = Some(voluntary);
            sample.nonvoluntary = Some(nonvoluntary);
        }
        if read_task_file(&mut path, "stat", &mut text).is_ok()
            && let Some(utime) = parse_stat_utime(&text)
        {
            sample.utime_ticks = Some(utime);
        }
        if read_task_file(&mut path, "syscall", &mut text).is_ok() {
            (sample.syscall, sample.arg0) = parse_syscall(&text);
            if let (Some(number), Some(fd)) = (sample.syscall, sample.arg0)
                && syscall_arg0_is_fd(number)
                && let Ok(target) = std::fs::read_link(format!("/proc/self/fd/{fd}"))
            {
                sample.fd_target_len = copy_text(&mut sample.fd_target, &target.to_string_lossy());
            }
        }
        if read_task_file(&mut path, "wchan", &mut text).is_ok() {
            sample.wchan_len = copy_text(&mut sample.wchan, text.trim());
        }
        out.push(sample);
        path.pop();
    }
}

#[cfg(target_os = "linux")]
fn read_task_file(
    path: &mut std::path::PathBuf,
    name: &str,
    text: &mut String,
) -> std::io::Result<()> {
    use std::io::Read as _;

    path.push(name);
    text.clear();
    let result = std::fs::File::open(&*path).and_then(|mut file| file.read_to_string(text));
    path.pop();
    result.map(|_| ())
}

#[cfg(target_os = "linux")]
pub(crate) fn clock_ticks_per_second() -> u64 {
    let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    u64::try_from(ticks).unwrap_or(100)
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn sample_self(out: &mut Vec<ThreadSample>, _cap: usize) {
    out.clear();
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn clock_ticks_per_second() -> u64 {
    100
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn syscall_arg0_is_fd(number: i64) -> bool {
    matches!(number, 0 | 17 | 19 | 45 | 47 | 232 | 281)
}

#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
fn syscall_arg0_is_fd(number: i64) -> bool {
    matches!(number, 63 | 65 | 67 | 207 | 212 | 22)
}

#[cfg(all(
    target_os = "linux",
    not(any(target_arch = "x86_64", target_arch = "aarch64"))
))]
fn syscall_arg0_is_fd(_number: i64) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::{parse_stat_utime, parse_status, parse_syscall};

    #[cfg(target_os = "linux")]
    use super::{ThreadSample, sample_self};

    #[test]
    fn parses_context_switch_counters() {
        assert_eq!(
            parse_status(
                "Name:\ttest\nvoluntary_ctxt_switches:\t19\nnonvoluntary_ctxt_switches:\t4\n"
            ),
            Some((19, 4))
        );
        assert_eq!(parse_status("voluntary_ctxt_switches: 1\n"), None);
    }

    #[test]
    fn parses_running_and_futex_syscall_files() {
        assert_eq!(parse_syscall("running"), (None, None));
        assert_eq!(
            parse_syscall("202 0x7ffd1234 0x80 0x0"),
            (Some(202), Some(0x7ffd1234))
        );
    }

    #[test]
    fn parses_stat_utime_after_a_parenthesized_comm() {
        let stat = "42 (comm with ) paren) S 1 2 3 4 5 6 7 8 9 10 123 13 14";
        assert_eq!(parse_stat_utime(stat), Some(123));
        assert_eq!(parse_stat_utime("malformed"), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn samples_the_named_calling_thread() {
        let thread = thegn_core::thread_name::spawn_named("thegn-census", || {
            crate::platform::qos::set_self(crate::platform::qos::Qos::Background);
            let mut samples = Vec::<ThreadSample>::with_capacity(1);
            sample_self(&mut samples, 256);
            samples
                .into_iter()
                .find(|sample| sample.comm_str() == "thegn-census")
        });
        let sample = thread.join().unwrap();
        assert!(
            sample.is_some(),
            "the census should include its named caller thread"
        );
    }
}
