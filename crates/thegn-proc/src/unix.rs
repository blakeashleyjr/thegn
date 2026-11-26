use crate::{
    BACKOFF_CAP, Capture, Containment, End, GroupState, InputFeed, Leader, LeaderExit, Limits,
    POLL_INTERVAL, Run, StdinHandoff, Stream, StreamConsumer, Which, aggregate_fit,
};
#[cfg(any(test, not(target_os = "macos")))]
use nix::errno::Errno;
use nix::{
    sys::signal::{Signal, killpg},
    unistd::Pid,
};
use std::{
    fs::File,
    io,
    os::{
        fd::{AsRawFd, FromRawFd, IntoRawFd},
        unix::process::CommandExt,
    },
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

#[cfg(target_os = "linux")]
use std::path::{Path, PathBuf};

struct Guard {
    pid: libc::pid_t,
    active: bool,
    /// The leader's status once [`own_group_live`] reaped it early (THE-1996). From then on its
    /// pid no longer pins the pgid, so destructive signals go through [`signal_group`].
    reaped: Option<Leader>,
}

impl Drop for Guard {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        if self.reaped.is_some() {
            // The leader is already reaped: never waitpid (the pid may be someone else's now) and
            // kill only on a fresh /proc confirmation that the group is still ours.
            if group_confirmed_after_reap(self.pid) {
                send_group_signal(self.pid, Signal::SIGKILL);
            }
            return;
        }
        let pgid = Pid::from_raw(self.pid);
        if let Err(_gone) = killpg(pgid, Signal::SIGKILL) {
            // The group may already be empty while unwinding; still reap the pinned leader below.
        }
        // SAFETY: this guard owns the spawned child and is the sole waitpid caller during unwind.
        unsafe { libc::waitpid(self.pid, std::ptr::null_mut(), 0) };
    }
}

impl Guard {
    fn disarm(&mut self) {
        self.active = false;
    }
}

struct Pipes<'a> {
    stdout: Option<File>,
    stderr: Option<File>,
    out_open: bool,
    err_open: bool,
    feed: Feed<'a>,
    /// The one consumer for every `Stream::Consume` stream; each Accumulator only flags whether
    /// its stream is delivered (two streams cannot each hold this `&mut`).
    consume: StreamConsumer<'a>,
}

struct Feed<'a> {
    stdin: Option<File>,
    input: &'a [u8],
    pos: usize,
    state: InputFeed,
}

const FEED_PUMP_CAP: usize = 1 << 20;

impl Feed<'_> {
    fn pending(&self) -> bool {
        self.stdin.is_some()
    }

    fn pump(&mut self) {
        let mut written_this_call: usize = 0;
        while self.stdin.is_some() {
            if self.pos == self.input.len() {
                self.close();
                self.state.complete = true;
                return;
            }
            if written_this_call >= FEED_PUMP_CAP {
                return;
            }
            let remaining = (self.input.len() - self.pos).min(FEED_PUMP_CAP - written_this_call);
            let fd = self.stdin.as_ref().map_or(-1, AsRawFd::as_raw_fd);
            // SAFETY: stdin is a live owned descriptor and the input slice is valid for remaining bytes.
            let count =
                unsafe { libc::write(fd, self.input[self.pos..].as_ptr().cast(), remaining) };
            if count > 0 {
                let count = count as usize;
                self.pos += count;
                written_this_call += count;
                self.state.written = self.state.written.saturating_add(count as u64);
                continue;
            }
            if count == 0 {
                self.close();
                self.state.error = Some(io::ErrorKind::WriteZero);
                return;
            }
            let error = io::Error::last_os_error();
            match error.kind() {
                io::ErrorKind::Interrupted => {}
                io::ErrorKind::WouldBlock => return,
                kind => {
                    self.close();
                    self.state.error = Some(kind);
                    return;
                }
            }
        }
    }

    fn close(&mut self) {
        self.stdin = None;
    }
}

struct Accumulator {
    capture: Capture,
    stream: Stream,
    which: Which,
    /// This stream is `Stream::Consume`: hand every chunk to the run's consumer.
    consume: bool,
}

impl Accumulator {
    fn read(
        &mut self,
        fd: libc::c_int,
        consume: &mut dyn FnMut(Which, &[u8]),
    ) -> io::Result<(bool, bool)> {
        let mut buf = [0_u8; 8192];
        let mut drained = 0;
        loop {
            // SAFETY: fd is an owned, nonblocking pipe descriptor and buf is writable for its length.
            let count = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
            if count == 0 {
                self.capture.eof = true;
                return Ok((false, false));
            }
            if count < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                if error.kind() == io::ErrorKind::WouldBlock {
                    return Ok((false, false));
                }
                return Err(error);
            }
            let bytes = &buf[..count as usize];
            drained += bytes.len();
            self.capture.total = self.capture.total.saturating_add(bytes.len() as u64);
            if self.consume {
                consume(self.which, bytes);
            }
            match self.stream {
                Stream::Null => {}
                Stream::Head { cap } => {
                    let remaining = cap.saturating_sub(self.capture.bytes.len());
                    self.capture
                        .bytes
                        .extend_from_slice(&bytes[..bytes.len().min(remaining)]);
                    if bytes.len() > remaining {
                        self.capture.truncated = true;
                    }
                }
                Stream::HeadOrFail { cap } => {
                    let remaining = cap.saturating_sub(self.capture.bytes.len());
                    self.capture
                        .bytes
                        .extend_from_slice(&bytes[..bytes.len().min(remaining)]);
                    if bytes.len() > remaining {
                        self.capture.truncated = true;
                        return Ok((true, true));
                    }
                }
                Stream::Tail { cap } => {
                    self.capture.bytes.extend_from_slice(bytes);
                    if self.capture.bytes.len() > cap {
                        let excess = self.capture.bytes.len() - cap;
                        self.capture.bytes.drain(..excess);
                        self.capture.truncated = true;
                    }
                }
                Stream::Consume => {}
            }
            if drained >= 1024 * 1024 {
                return Ok((true, false));
            }
        }
    }
}

fn set_nonblocking(file: &File) -> io::Result<()> {
    // SAFETY: fcntl operates on the live descriptor owned by file.
    let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: this updates only the status flags on the owned pipe descriptor.
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn set_blocking(file: &File) -> io::Result<()> {
    // SAFETY: fcntl operates on the live descriptor owned by file.
    let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: this updates only the status flags on the owned pipe descriptor.
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFL, flags & !libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Detach: a survivor still holds this pipe. Closing the read end would SIGPIPE it on its next
/// write, so a one-shot named thread reads and discards until EOF (the survivor exits or closes
/// it). If the thread cannot be started the pipe is closed, as before.
fn sink_abandoned(file: File) {
    if let Err(_error) = set_blocking(&file) {
        return;
    }
    if let Err(_spawn_error) = std::thread::Builder::new()
        .name("thegn-proc-sink".to_owned())
        .spawn(move || {
            let mut file = file;
            if let Err(_error) = io::copy(&mut file, &mut io::sink()) {
                // The survivor's end failed or closed; dropping the read end is all that is left.
            }
        })
    {
        // Spawn failure drops the closure and with it the pipe: the pre-sink behaviour.
    }
}

fn leader_exited(pid: libc::pid_t) -> io::Result<bool> {
    let mut info = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
    // SAFETY: waitid initializes siginfo on success. WNOWAIT leaves the leader waitable and pins its pid.
    let result = unsafe {
        libc::waitid(
            libc::P_PID,
            pid as libc::id_t,
            info.as_mut_ptr(),
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        // SAFETY: successful waitid initialized the siginfo structure.
        Ok(unsafe { info.assume_init().si_pid() } != 0)
    }
}

#[cfg(target_os = "linux")]
const GLOBAL_SCAN_CACHE: Duration = Duration::from_millis(50);

#[cfg(all(test, target_os = "linux"))]
thread_local! {
    static PROC_READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static GLOBAL_PROC_SCANS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[derive(Default)]
struct LivenessScan {
    #[cfg(target_os = "linux")]
    global: Option<(PathBuf, libc::pid_t, Instant, bool)>,
}

#[cfg(target_os = "linux")]
fn proc_read_dir(path: &Path) -> io::Result<std::fs::ReadDir> {
    #[cfg(test)]
    PROC_READS.with(|reads| reads.set(reads.get().saturating_add(1)));
    std::fs::read_dir(path)
}

#[cfg(target_os = "linux")]
fn proc_read_to_string(path: &Path) -> io::Result<String> {
    #[cfg(test)]
    PROC_READS.with(|reads| reads.set(reads.get().saturating_add(1)));
    std::fs::read_to_string(path)
}

#[cfg(target_os = "linux")]
fn proc_stat_state_pgrp(stat: &str) -> Option<(u8, libc::pid_t)> {
    let (_, rest) = stat.rsplit_once(')')?;
    let mut fields = rest.split_whitespace();
    let state = *fields.next()?.as_bytes().first()?;
    let _ppid = fields.next()?;
    let pgrp = fields.next()?.parse().ok()?;
    Some((state, pgrp))
}

#[cfg(target_os = "linux")]
fn proc_stat_live_in_group(stat: &str, pid: libc::pid_t) -> Option<bool> {
    let (state, pgrp) = proc_stat_state_pgrp(stat)?;
    Some(state != b'Z' && state != b'X' && pgrp == pid)
}

#[cfg(target_os = "linux")]
fn proc_stat_is_live(stat: &str) -> Option<bool> {
    let (state, _) = proc_stat_state_pgrp(stat)?;
    Some(state != b'Z' && state != b'X')
}

#[cfg(target_os = "linux")]
fn global_group_scan(proc_root: &Path, pid: libc::pid_t) -> bool {
    #[cfg(test)]
    GLOBAL_PROC_SCANS.with(|scans| scans.set(scans.get().saturating_add(1)));
    let Ok(entries) = proc_read_dir(proc_root) else {
        return false;
    };
    for entry in entries.flatten() {
        let Some(member) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<libc::pid_t>().ok())
        else {
            continue;
        };
        let Ok(stat) = proc_read_to_string(&entry.path().join("stat")) else {
            continue;
        };
        if (member == pid && proc_stat_is_live(&stat).is_some_and(|live| live))
            || (member != pid && proc_stat_live_in_group(&stat, pid) == Some(true))
        {
            return true;
        }
    }
    false
}

#[cfg(target_os = "linux")]
fn cached_global_group_scan(proc_root: &Path, pid: libc::pid_t, scan: &mut LivenessScan) -> bool {
    if let Some((root, cached_pid, at, result)) = &scan.global
        && root == proc_root
        && *cached_pid == pid
        && at.elapsed() <= GLOBAL_SCAN_CACHE
    {
        return *result;
    }
    let result = global_group_scan(proc_root, pid);
    scan.global = Some((proc_root.to_path_buf(), pid, Instant::now(), result));
    result
}

#[cfg(target_os = "linux")]
fn group_live_with_scan(proc_root: &Path, pid: libc::pid_t, scan: &mut LivenessScan) -> bool {
    if !group_exists(pid) {
        return false;
    }
    let leader_stat = proc_read_to_string(&proc_root.join(pid.to_string()).join("stat"));
    if let Ok(stat) = &leader_stat
        && proc_stat_is_live(stat).is_some_and(|live| live)
    {
        return true;
    }
    cached_global_group_scan(proc_root, pid, scan)
}

#[cfg(target_os = "linux")]
fn proc_stat_live_in_session_group(
    stat: &str,
    pid: libc::pid_t,
    session: libc::pid_t,
) -> Option<bool> {
    let (_, rest) = stat.rsplit_once(')')?;
    let mut fields = rest.split_whitespace();
    let state = *fields.next()?.as_bytes().first()?;
    let _ppid = fields.next()?;
    let pgrp: libc::pid_t = fields.next()?.parse().ok()?;
    let member_session: libc::pid_t = fields.next()?.parse().ok()?;
    Some(state != b'Z' && state != b'X' && pgrp == pid && member_session == session)
}

/// Fresh (uncached) `/proc` walk for a live process in group `pid` and session `session`.
#[cfg(target_os = "linux")]
fn group_confirmed_in(proc_root: &Path, pid: libc::pid_t, session: libc::pid_t) -> bool {
    #[cfg(test)]
    GLOBAL_PROC_SCANS.with(|scans| scans.set(scans.get().saturating_add(1)));
    let Ok(entries) = proc_read_dir(proc_root) else {
        return false;
    };
    entries.flatten().any(|entry| {
        let numeric = entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.parse::<libc::pid_t>().is_ok());
        numeric
            && proc_read_to_string(&entry.path().join("stat")).is_ok_and(|stat| {
                proc_stat_live_in_session_group(&stat, pid, session) == Some(true)
            })
    })
}

/// The identity check a destructive signal needs once the leader has been reaped early.
///
/// While the leader is alive or an unreaped zombie its pid pins the pgid, so `killpg` cannot
/// reach a recycled group. After the reap that pin is gone; a post-reap `killpg(pgid, 0)` only
/// answers "keep waiting?". Before SIGTERM/SIGKILL we therefore re-establish, immediately before
/// signalling, that a live process still carries this pgid inside our own session (the spawned
/// group never leaves it: `setsid` would take a member out of the group too). This is the rare
/// survivors path; the common empty-group path never gets here.
#[cfg(target_os = "linux")]
fn group_confirmed_after_reap(pid: libc::pid_t) -> bool {
    // SAFETY: getsid(0) only queries the calling process's own session id.
    let session = unsafe { libc::getsid(0) };
    session >= 0 && group_confirmed_in(Path::new("/proc"), pid, session)
}

/// Non-Linux never reaps the leader early (see [`own_group_live`]); refuse rather than signal an
/// unpinned pgid should that ever change.
#[cfg(not(target_os = "linux"))]
fn group_confirmed_after_reap(_pid: libc::pid_t) -> bool {
    false
}

#[cfg(target_os = "linux")]
/// Return whether the process group has a live member using the cached `/proc`
/// liveness scan used by the supervisor.
///
/// This is the fallback for a group whose leader the caller cannot reap (it is not the caller's
/// child, e.g. a detached survivor group): its leader may be a zombie that keeps `killpg(pgid, 0)`
/// succeeding, so emptiness needs the global `/proc` walk. A run's own group uses the cheaper
/// reap-then-probe in `own_group_live`.
pub fn group_has_live_members(pgid: u32) -> bool {
    let mut scan = LivenessScan::default();
    group_live_with_scan(Path::new("/proc"), pgid as libc::pid_t, &mut scan)
}

#[cfg(all(test, target_os = "linux"))]
fn group_has_live_members_in(proc_root: &Path, pid: libc::pid_t) -> bool {
    let mut scan = LivenessScan::default();
    group_live_with_scan(proc_root, pid, &mut scan)
}

#[cfg(any(test, all(not(target_os = "linux"), not(target_os = "macos"))))]
fn group_live(pid: libc::pid_t) -> bool {
    #[cfg(target_os = "linux")]
    {
        group_has_live_members_in(Path::new("/proc"), pid)
    }
    #[cfg(not(target_os = "linux"))]
    {
        group_exists(pid)
    }
}

#[cfg(any(test, not(target_os = "macos")))]
fn group_exists(pid: libc::pid_t) -> bool {
    match killpg(Pid::from_raw(pid), None) {
        Ok(()) => true,
        Err(Errno::ESRCH) => false,
        Err(_) => true,
    }
}

/// macOS `killpg(pgid, 0)` also sees an unreaped zombie leader, so it cannot
/// prove that the group's pipes have drained while we keep that leader pinned
/// for safe group signalling. libproc gives us the member pids and each
/// process's BSD state; a group containing only zombies has no live work left.
/// Any incomplete or denied observation fails closed as live.
#[cfg(target_os = "macos")]
fn macos_group_has_live_members(
    pgid: libc::pid_t,
    pinned_exited_leader: Option<libc::pid_t>,
) -> bool {
    const MAX_GROUP_MEMBERS: usize = 1024;

    let mut pids = [0 as libc::pid_t; MAX_GROUP_MEMBERS];
    // SAFETY: libproc writes no more than the byte length of the owned buffer.
    // Its return value is a PID count; a full buffer may be truncated, so treat
    // that observation as unknown/live rather than claiming the group drained.
    let count = unsafe {
        libc::proc_listpgrppids(
            pgid,
            pids.as_mut_ptr().cast(),
            std::mem::size_of_val(&pids) as libc::c_int,
        )
    };
    if count < 0 || count as usize >= pids.len() {
        return true;
    }

    pids[..count as usize].iter().any(|&pid| {
        if pid <= 0 {
            return true;
        }
        // `waitid(WNOWAIT)` proves this exact owned child has exited while
        // keeping its PID pinned for safe group signalling. libproc can list
        // that zombie but cannot inspect it (`ESRCH`), so it is the only
        // listed member that may be skipped without a state read.
        if pinned_exited_leader == Some(pid) {
            return false;
        }
        // SAFETY: zero is a valid initial value, and libproc is given the exact
        // size of the caller-owned structure.
        let mut info: libc::proc_bsdshortinfo = unsafe { std::mem::zeroed() };
        let want = std::mem::size_of::<libc::proc_bsdshortinfo>() as libc::c_int;
        // SAFETY: `info` is writable for `want` bytes for the duration of call.
        let got = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDT_SHORTBSDINFO,
                0,
                std::ptr::from_mut(&mut info).cast(),
                want,
            )
        };
        if got != want {
            // The member may have raced with exit or may be unobservable. Either
            // way, another bounded poll is safer than proving emptiness.
            true
        } else if info.pbsi_pgid != pgid as u32 {
            // Membership changed between enumeration and inspection; keep the
            // group owned for another bounded observation.
            true
        } else {
            macos_process_status_is_live(info.pbsi_status)
        }
    })
}

#[cfg(target_os = "macos")]
fn macos_process_status_is_live(status: u32) -> bool {
    status != libc::SZOMB
}

fn wait_leader(child: &mut Child) -> Leader {
    match child.wait() {
        Ok(status) => Leader::Reaped(status),
        Err(error) => Leader::ReapFailed(error),
    }
}

/// Whether the run's own process group still has a live member.
///
/// - Leader alive: the group is live; no `/proc` read.
/// - Linux leader exited: reap it now (its status is kept in `guard.reaped` and returned by
///   [`reap_after`] unchanged), then probe at once with `killpg(pgid, 0)`: `ESRCH` proves the group
///   empty, success means members remain. An unreaped zombie leader would keep that probe
///   succeeding, which is what forced the old global `/proc` walk.
/// - Accepted risk: once the group drains, its pgid could in principle be recycled before the
///   next probe. A post-reap probe therefore only answers "keep waiting?"; destructive signals go
///   through [`signal_group`], which re-confirms the group first.
/// - Reaping impossible (`waitid`/`waitpid` fails, i.e. the leader is not our waitable child): the
///   cached global `/proc` scan answers instead, as it does for [`group_has_live_members`].
///
/// macOS keeps the leader pinned and uses libproc to ignore zombie-only membership; other
/// non-Linux Unix targets keep the leader pinned and use `killpg(pgid, 0)` conservatively.
fn own_group_live(
    guard: &mut Guard,
    #[cfg_attr(not(target_os = "linux"), allow(unused_variables))] child: &mut Child,
    #[cfg_attr(not(target_os = "linux"), allow(unused_variables))] liveness: &mut LivenessScan,
) -> bool {
    #[cfg(not(target_os = "linux"))]
    {
        #[cfg(target_os = "macos")]
        {
            match leader_exited(guard.pid) {
                Ok(true) => macos_group_has_live_members(guard.pid, Some(guard.pid)),
                Ok(false) => macos_group_has_live_members(guard.pid, None),
                Err(_error) => true,
            }
        }
        #[cfg(not(target_os = "macos"))]
        {
            group_live(guard.pid)
        }
    }
    #[cfg(target_os = "linux")]
    {
        if guard.reaped.is_none() {
            match leader_exited(guard.pid) {
                Ok(false) => return true,
                Ok(true) => guard.reaped = Some(wait_leader(child)),
                Err(_not_waitable) => {
                    return group_live_with_scan(Path::new("/proc"), guard.pid, liveness);
                }
            }
        }
        if matches!(guard.reaped, Some(Leader::ReapFailed(_))) {
            // The reap failed, so the leader may still be a pinned zombie: only the scan can tell.
            return group_live_with_scan(Path::new("/proc"), guard.pid, liveness);
        }
        group_exists(guard.pid)
    }
}

fn poll_pipes(pipes: &Pipes<'_>, timeout: Duration) -> io::Result<bool> {
    let mut fds = [
        libc::pollfd {
            fd: pipes
                .stdout
                .as_ref()
                .filter(|_| pipes.out_open)
                .map_or(-1, AsRawFd::as_raw_fd),
            events: libc::POLLIN | libc::POLLHUP,
            revents: 0,
        },
        libc::pollfd {
            fd: pipes
                .feed
                .stdin
                .as_ref()
                .filter(|_| pipes.feed.pending())
                .map_or(-1, AsRawFd::as_raw_fd),
            events: libc::POLLOUT,
            revents: 0,
        },
        libc::pollfd {
            fd: pipes
                .stderr
                .as_ref()
                .filter(|_| pipes.err_open)
                .map_or(-1, AsRawFd::as_raw_fd),
            events: libc::POLLIN | libc::POLLHUP,
            revents: 0,
        },
    ];
    let millis = timeout.as_millis().min(i32::MAX as u128) as i32;
    // SAFETY: fds points to three initialized pollfd records for the duration of poll.
    let result = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, millis) };
    if result < 0 {
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::Interrupted {
            Ok(false)
        } else {
            Err(error)
        }
    } else {
        Ok(result > 0)
    }
}

fn drain(
    pipes: &mut Pipes<'_>,
    out: &mut Accumulator,
    err: &mut Accumulator,
) -> io::Result<Option<Which>> {
    pipes.feed.pump();
    let mut result = None;
    if pipes.out_open
        && let Some(pipe) = &pipes.stdout
    {
        let (_, overflow) = match out.read(pipe.as_raw_fd(), &mut *pipes.consume) {
            Ok(result) => result,
            Err(_error) => {
                pipes.out_open = false;
                return Ok(result);
            }
        };
        if out.capture.eof {
            pipes.out_open = false;
        }
        if overflow {
            result = Some(Which::Stdout);
        }
    }
    if pipes.err_open
        && let Some(pipe) = &pipes.stderr
    {
        let (_, overflow) = match err.read(pipe.as_raw_fd(), &mut *pipes.consume) {
            Ok(result) => result,
            Err(_error) => {
                pipes.err_open = false;
                return Ok(result);
            }
        };
        if err.capture.eof {
            pipes.err_open = false;
        }
        if overflow && result.is_none() {
            result = Some(Which::Stderr);
        }
    }
    Ok(result)
}

fn nap(pipes: &mut Pipes<'_>, backoff: Duration) -> io::Result<()> {
    if pipes.out_open || pipes.err_open || pipes.feed.pending() {
        match poll_pipes(pipes, backoff) {
            Ok(_) => {}
            Err(error) => {
                pipes.out_open = false;
                pipes.err_open = false;
                pipes.feed.close();
                pipes.feed.state.error = Some(error.kind());
                std::thread::sleep(backoff);
            }
        }
    } else {
        std::thread::sleep(backoff);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn wait_empty(
    guard: &mut Guard,
    child: &mut Child,
    deadline: Instant,
    pipes: &mut Pipes<'_>,
    out: &mut Accumulator,
    err: &mut Accumulator,
    backoff: &mut Duration,
    liveness: &mut LivenessScan,
) -> io::Result<bool> {
    loop {
        if !own_group_live(guard, child, liveness) {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        let before = out.capture.total.saturating_add(err.capture.total);
        drain(pipes, out, err)?;
        if out.capture.total.saturating_add(err.capture.total) > before {
            *backoff = POLL_INTERVAL;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        nap(pipes, (*backoff).min(remaining))?;
        *backoff = crate::next_backoff(*backoff, remaining);
    }
}

fn send_group_signal(pid: libc::pid_t, signal: Signal) {
    if let Err(_gone) = killpg(Pid::from_raw(pid), signal) {
        // ESRCH means the group drained between its liveness check and this best-effort signal.
    }
}

/// Send a destructive signal to the run's group. While the leader is unreaped its pid pins the
/// pgid (the pre-THE-1996 identity guarantee, kept). Once [`own_group_live`] has reaped it, the
/// signal is sent only after [`group_confirmed_after_reap`] re-confirms the group, never on the
/// strength of a post-reap `killpg(pgid, 0)` alone.
fn signal_group(guard: &Guard, signal: Signal) {
    if guard.reaped.is_some() && !group_confirmed_after_reap(guard.pid) {
        return;
    }
    send_group_signal(guard.pid, signal);
}

fn reap_after(
    child: &mut Child,
    guard: &mut Guard,
    kill_settle: Duration,
    signalled: bool,
) -> Leader {
    if let Some(leader) = guard.reaped.take() {
        // Reaped early by `own_group_live`: this is the leader's own status, unchanged.
        return leader;
    }
    let pid = guard.pid;
    if !signalled {
        return match child.try_wait() {
            Ok(Some(status)) => Leader::Reaped(status),
            Ok(None) => detach(child),
            Err(error) => Leader::ReapFailed(error),
        };
    }
    let deadline = Instant::now() + kill_settle;
    loop {
        match leader_exited(pid) {
            Ok(true) => return wait_leader(child),
            Ok(false) if Instant::now() < deadline => std::thread::sleep(POLL_INTERVAL),
            Ok(false) => return detach(child),
            Err(error) => return Leader::ReapFailed(error),
        }
    }
}

fn detach(child: &mut Child) -> Leader {
    let pid = child.id();
    let result = std::thread::Builder::new()
        .name("thegn-proc-reap".to_owned())
        .spawn(move || {
            // SAFETY: this thread takes sole ownership of waiting for the detached child process.
            unsafe { libc::waitpid(pid as libc::pid_t, std::ptr::null_mut(), 0) };
        });
    match result {
        Ok(handle) => {
            drop(handle);
            Leader::Detached
        }
        Err(_spawn_error) => match child.wait() {
            Ok(status) => Leader::Reaped(status),
            Err(error) => Leader::ReapFailed(error),
        },
    }
}

#[cfg(test)]
pub(super) fn run(cmd: &mut Command, limits: &Limits, cancel: &AtomicBool) -> Run {
    run_consuming_streams(cmd, limits, cancel, &mut |_which: Which, _bytes: &[u8]| {})
}

pub(super) fn run_consuming_streams(
    cmd: &mut Command,
    limits: &Limits,
    cancel: &AtomicBool,
    consume: StreamConsumer<'_>,
) -> Run {
    run_inner(cmd, limits, cancel, consume, None, None).0
}

pub(super) fn run_consuming_streams_with_stdin(
    cmd: &mut Command,
    limits: &Limits,
    cancel: &AtomicBool,
    on_stdin: StdinHandoff<'_>,
    consume: StreamConsumer<'_>,
) -> Run {
    run_inner(cmd, limits, cancel, consume, None, Some(on_stdin)).0
}

pub(super) fn run_with_input(
    cmd: &mut Command,
    limits: &Limits,
    input: &[u8],
    cancel: &AtomicBool,
) -> (Run, InputFeed) {
    run_inner(
        cmd,
        limits,
        cancel,
        &mut |_which: Which, _bytes: &[u8]| {},
        Some(input),
        None,
    )
}

/// `input` (a payload the poll loop feeds) and `handoff` (the caller takes the pipe) are never both
/// set by the public entry points; either one makes stdin a pipe.
fn run_inner(
    cmd: &mut Command,
    limits: &Limits,
    cancel: &AtomicBool,
    consume: StreamConsumer<'_>,
    input: Option<&[u8]>,
    handoff: Option<StdinHandoff<'_>>,
) -> (Run, InputFeed) {
    cmd.stdin(if input.is_some() || handoff.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    cmd.stdout(if matches!(limits.stdout, Stream::Null) {
        Stdio::null()
    } else {
        Stdio::piped()
    });
    cmd.stderr(if matches!(limits.stderr, Stream::Null) {
        Stdio::null()
    } else {
        Stdio::piped()
    });
    cmd.process_group(0);
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(error) => {
            return (
                Run {
                    end: End::Spawn(error),
                    group: GroupState::Unknown,
                    leader: Leader::NotSpawned,
                    leader_exited_first: false,
                    stdout: Capture::default(),
                    stderr: Capture::default(),
                    containment: Containment::ProcessGroup,
                    idle_expired: false,
                },
                InputFeed::default(),
            );
        }
    };
    let pid = child.id() as libc::pid_t;
    let mut guard = Guard {
        pid,
        active: true,
        reaped: None,
    };
    let stdout = child.stdout.take().map(|pipe| {
        let fd = pipe.into_raw_fd();
        // SAFETY: ownership of this raw descriptor is transferred from ChildStdout to File.
        unsafe { File::from_raw_fd(fd) }
    });
    let stderr = child.stderr.take().map(|pipe| {
        let fd = pipe.into_raw_fd();
        // SAFETY: ownership of this raw descriptor is transferred from ChildStderr to File.
        unsafe { File::from_raw_fd(fd) }
    });
    let mut stdin = child.stdin.take();
    if let Some(handoff) = handoff
        && let Some(pipe) = stdin.take()
    {
        // The group exists and the Guard is armed, so a panicking handoff still kills and reaps.
        // The pipe stays blocking: it is the caller's from here on.
        handoff(pipe);
    }
    let stdin = stdin.map(|pipe| {
        let fd = pipe.into_raw_fd();
        // SAFETY: ownership of this raw descriptor is transferred from ChildStdin to File.
        unsafe { File::from_raw_fd(fd) }
    });
    let input_bytes = input.unwrap_or(&[]);
    let mut feed = Feed {
        stdin,
        input: input_bytes,
        pos: 0,
        state: InputFeed::default(),
    };
    if input.is_some() && input_bytes.is_empty() {
        feed.close();
        feed.state.complete = true;
    }
    let mut pipes = Pipes {
        out_open: stdout.is_some(),
        err_open: stderr.is_some(),
        stdout,
        stderr,
        feed,
        consume,
    };
    let setup_error = [&pipes.stdout, &pipes.stderr, &pipes.feed.stdin]
        .into_iter()
        .flatten()
        .find_map(|pipe| set_nonblocking(pipe).err());
    if let Some(error) = setup_error {
        if pipes.feed.pending() {
            pipes.feed.state.error = Some(error.kind());
        }
        pipes.feed.close();
        send_group_signal(pid, Signal::SIGKILL);
        if let Err(_kill_error) = child.kill() {
            // The group signal above is the primary cleanup; wait still reaps the leader.
        }
        let leader = match child.wait() {
            Ok(status) => Leader::Reaped(status),
            Err(wait_error) => Leader::ReapFailed(wait_error),
        };
        guard.disarm();
        return (
            Run {
                end: End::TimedOut,
                group: GroupState::Terminated,
                leader,
                leader_exited_first: false,
                stdout: Capture::default(),
                stderr: Capture::default(),
                containment: Containment::ProcessGroup,
                idle_expired: false,
            },
            pipes.feed.state,
        );
    }
    let start = Instant::now();
    let deadline = start + limits.timeout;
    let mut out = Accumulator {
        capture: Capture::default(),
        stream: limits.stdout,
        which: Which::Stdout,
        consume: matches!(limits.stdout, Stream::Consume),
    };
    let mut err = Accumulator {
        capture: Capture::default(),
        stream: limits.stderr,
        which: Which::Stderr,
        consume: matches!(limits.stderr, Stream::Consume),
    };
    let mut backoff = POLL_INTERVAL;
    let mut liveness = LivenessScan::default();
    let mut leader_first = false;
    // Idle-output clock: starts at spawn, reset by every chunk read before the leader exits.
    let mut last_output = start;
    let mut idle_expired = false;
    let mut end = loop {
        if cancel.load(Ordering::Acquire) {
            break End::Cancelled;
        }
        match leader_exited(pid) {
            Ok(true) => {
                leader_first = true;
                if limits.leader_exit == LeaderExit::Detach {
                    // Detach: no quiesce window. One last drain (the leader is still a pinned
                    // zombie, so an overflow here can still terminate the group safely), then the
                    // run ends; the members left are not signalled.
                    break match drain(&mut pipes, &mut out, &mut err) {
                        Ok(Some(which)) => End::OutputLimit(which),
                        Ok(None) | Err(_) => End::LeaderExited,
                    };
                }
                let quiesce = limits
                    .quiesce_after_leader
                    .map_or(deadline, |span| deadline.min(Instant::now() + span));
                let quiesce_end = loop {
                    if cancel.load(Ordering::Acquire) {
                        break End::Cancelled;
                    }
                    let before = out.capture.total.saturating_add(err.capture.total);
                    match drain(&mut pipes, &mut out, &mut err) {
                        Ok(Some(which)) => break End::OutputLimit(which),
                        Ok(None) => {}
                        Err(_error) => {}
                    }
                    if !own_group_live(&mut guard, &mut child, &mut liveness) {
                        break End::LeaderExited;
                    }
                    if out.capture.total.saturating_add(err.capture.total) > before {
                        backoff = POLL_INTERVAL;
                    }
                    if Instant::now() >= quiesce {
                        break End::LeaderExited;
                    }
                    let remaining = quiesce.saturating_duration_since(Instant::now());
                    if let Err(_error) = nap(&mut pipes, backoff.min(remaining)) {}
                    backoff = crate::next_backoff(backoff, remaining);
                };
                break quiesce_end;
            }
            Ok(false) => {}
            Err(error) => break End::Supervise(error),
        }
        if Instant::now() >= deadline {
            break End::TimedOut;
        }
        let before = out.capture.total.saturating_add(err.capture.total);
        if let Some(which) = drain(&mut pipes, &mut out, &mut err).unwrap_or_default() {
            break End::OutputLimit(which);
        }
        if out.capture.total.saturating_add(err.capture.total) > before {
            backoff = POLL_INTERVAL;
            last_output = Instant::now();
        }
        // Checked after the drain, so output that arrived during the last nap resets the clock
        // before it can expire.
        let idle_deadline = limits
            .idle_output
            .and_then(|span| last_output.checked_add(span));
        if idle_deadline.is_some_and(|idle| Instant::now() >= idle) {
            idle_expired = true;
            break End::TimedOut;
        }
        // The nap wakes for whichever bound comes first; the timeout check above still runs first.
        let wake = idle_deadline.map_or(deadline, |idle| deadline.min(idle));
        let remaining = wake.saturating_duration_since(Instant::now());
        if let Err(_error) = nap(&mut pipes, backoff.min(remaining)) {}
        backoff = crate::next_backoff(backoff, remaining);
    };
    pipes.feed.close();
    let mut group = GroupState::Unknown;
    let mut signalled = false;
    if !matches!(end, End::LeaderExited | End::Supervise(_)) {
        signalled = true;
        if limits.term_grace > Duration::ZERO {
            signal_group(&guard, Signal::SIGTERM);
            match wait_empty(
                &mut guard,
                &mut child,
                Instant::now() + limits.term_grace,
                &mut pipes,
                &mut out,
                &mut err,
                &mut backoff,
                &mut liveness,
            ) {
                Ok(true) => group = GroupState::Terminated,
                Ok(false) => {}
                Err(_error) => {}
            }
        }
        if group != GroupState::Terminated {
            signal_group(&guard, Signal::SIGKILL);
            group = match wait_empty(
                &mut guard,
                &mut child,
                Instant::now() + limits.kill_settle,
                &mut pipes,
                &mut out,
                &mut err,
                &mut backoff,
                &mut liveness,
            ) {
                Ok(true) => GroupState::Terminated,
                Ok(false) => GroupState::Survivors,
                Err(_error) => GroupState::Survivors,
            };
        }
    } else if matches!(&end, End::LeaderExited) {
        let group_is_live = own_group_live(&mut guard, &mut child, &mut liveness);
        group = match (group_is_live, limits.leader_exit) {
            (false, _) => GroupState::Drained,
            // Detach: never signal the members the leader left behind.
            (true, LeaderExit::Detach) => GroupState::Detached,
            (true, LeaderExit::Quiesce) => GroupState::Terminated,
        };
        if group == GroupState::Terminated {
            signalled = true;
            if limits.term_grace > Duration::ZERO {
                signal_group(&guard, Signal::SIGTERM);
                match wait_empty(
                    &mut guard,
                    &mut child,
                    Instant::now() + limits.term_grace,
                    &mut pipes,
                    &mut out,
                    &mut err,
                    &mut backoff,
                    &mut liveness,
                ) {
                    Ok(_) => {}
                    Err(_error) => {}
                }
            }
            signal_group(&guard, Signal::SIGKILL);
            match wait_empty(
                &mut guard,
                &mut child,
                Instant::now() + limits.kill_settle,
                &mut pipes,
                &mut out,
                &mut err,
                &mut backoff,
                &mut liveness,
            ) {
                Ok(false) => group = GroupState::Survivors,
                Ok(true) => group = GroupState::Terminated,
                Err(_error) => group = GroupState::Survivors,
            }
        }
    }
    let leader = reap_after(
        &mut child,
        &mut guard,
        limits.kill_settle,
        signalled || leader_first,
    );
    // The leader is reaped (or owned by the reap thread) from here on, so the guard has nothing
    // left to pin: its pgid may belong to detached survivors or be recycled. Disarm before the
    // reader-grace drain, so a consumer panic there cannot SIGKILL a detached group or waitpid a
    // reused pid.
    guard.disarm();
    let grace_end = Instant::now() + limits.reader_grace;
    while (pipes.out_open || pipes.err_open) && Instant::now() < grace_end {
        match drain(&mut pipes, &mut out, &mut err) {
            Ok(Some(which)) => end = End::OutputLimit(which),
            Ok(None) => {}
            Err(_error) => {}
        }
        let remaining = grace_end.saturating_duration_since(Instant::now());
        if let Err(_error) = nap(&mut pipes, BACKOFF_CAP.min(remaining)) {}
    }
    match drain(&mut pipes, &mut out, &mut err) {
        Ok(Some(which)) => end = End::OutputLimit(which),
        Ok(None) => {}
        Err(_error) => {}
    }
    if group == GroupState::Detached {
        if pipes.out_open
            && let Some(file) = pipes.stdout.take()
        {
            sink_abandoned(file);
        }
        if pipes.err_open
            && let Some(file) = pipes.stderr.take()
        {
            sink_abandoned(file);
        }
    }
    // A reader-grace OutputLimit overrides the end; the flag only ever accompanies TimedOut.
    let idle_expired = idle_expired && matches!(&end, End::TimedOut);
    let mut run = Run {
        end,
        group,
        leader,
        leader_exited_first: leader_first,
        stdout: out.capture,
        stderr: err.capture,
        containment: Containment::ProcessGroup,
        idle_expired,
    };
    aggregate_fit(
        &mut run.stdout,
        &mut run.stderr,
        limits.stdout,
        limits.stderr,
        limits.aggregate_tail_cap,
    );
    (run, pipes.feed.state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        io::Write,
        os::unix::fs::PermissionsExt,
        path::{Path, PathBuf},
        process::ChildStdin,
        sync::{Arc, atomic::AtomicBool, mpsc},
        thread,
    };

    use crate::run_consuming;

    struct Scripts(tempfile::TempDir);

    impl Scripts {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
            Self(dir)
        }
        fn write(&self, name: &str, body: &str) -> PathBuf {
            let path = self.0.path().join(name);
            fs::write(&path, body).unwrap();
            path
        }
        fn path(&self) -> &Path {
            self.0.path()
        }
    }

    #[cfg(target_os = "linux")]
    fn fake_proc_stat(pid: libc::pid_t, state: char, pgrp: libc::pid_t) -> String {
        format!("{pid} (fixture with spaces ) {state} 1 {pgrp} 0")
    }

    #[cfg(target_os = "linux")]
    fn fake_proc_process(root: &Path, pid: libc::pid_t, stat: &str, children: &str) {
        let task = root
            .join(pid.to_string())
            .join("task")
            .join(pid.to_string());
        fs::create_dir_all(&task).unwrap();
        fs::write(root.join(pid.to_string()).join("stat"), stat).unwrap();
        fs::write(task.join("children"), children).unwrap();
    }

    #[cfg(target_os = "linux")]
    fn fixture_group_pid() -> libc::pid_t {
        // The real group keeps killpg(pid, 0) successful while all /proc data
        // below is supplied by the fixture.
        // SAFETY: getpgrp takes no arguments, reads no memory and cannot fail.
        unsafe { libc::getpgrp() }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn group_has_live_members_zombies_are_dead_and_esrch_does_not_read_proc() {
        let root = tempfile::tempdir().unwrap();
        let pid = fixture_group_pid();
        fake_proc_process(root.path(), pid, &fake_proc_stat(pid, 'Z', pid), "201");
        fake_proc_process(root.path(), 201, &fake_proc_stat(201, 'Z', pid), "");
        GLOBAL_PROC_SCANS.with(|scans| scans.set(0));
        assert!(!group_has_live_members_in(root.path(), pid));
        GLOBAL_PROC_SCANS.with(|scans| assert_eq!(scans.get(), 1));

        PROC_READS.with(|reads| reads.set(0));
        assert!(!group_has_live_members_in(
            root.path(),
            i32::MAX as libc::pid_t
        ));
        PROC_READS.with(|reads| assert_eq!(reads.get(), 0));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn group_has_live_members_finds_reparented_member_once_per_scan_window() {
        let root = tempfile::tempdir().unwrap();
        let pid = fixture_group_pid();
        fake_proc_process(root.path(), pid, &fake_proc_stat(pid, 'Z', pid), "");
        fake_proc_process(root.path(), 301, &fake_proc_stat(301, 'S', pid), "");
        GLOBAL_PROC_SCANS.with(|scans| scans.set(0));
        let mut scan = LivenessScan::default();

        assert!(group_live_with_scan(root.path(), pid, &mut scan));
        assert!(group_live_with_scan(root.path(), pid, &mut scan));
        GLOBAL_PROC_SCANS.with(|scans| assert_eq!(scans.get(), 1));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn liveness_stat_parser_accepts_spaces_parentheses_and_rejects_truncated() {
        assert_eq!(
            proc_stat_state_pgrp("9 (name with ) spaces) S 1 9 0"),
            Some((b'S', 9))
        );
        assert_eq!(proc_stat_state_pgrp("9 (truncated) S"), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn real_process_group_is_live_then_dead_after_kill() {
        let mut command = Command::new("sh");
        command.args(["-c", "sleep 5 & wait"]);
        command.process_group(0);
        let mut child = command.spawn().unwrap();
        let pid = child.id() as libc::pid_t;
        assert!(group_live(pid));

        send_group_signal(pid, Signal::SIGKILL);
        child.wait().unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while group_live(pid) && Instant::now() < deadline {
            std::thread::sleep(POLL_INTERVAL);
        }
        assert!(!group_live(pid));
    }

    fn command(path: &Path) -> Command {
        let mut cmd = Command::new("sh");
        cmd.arg(path);
        cmd
    }

    fn proc_state_gone_or_zombie(pid: u32) -> bool {
        if !Path::new("/proc/self/stat").exists() {
            return true;
        }
        let stat_path = PathBuf::from(format!("/proc/{pid}/stat"));
        match fs::read_to_string(stat_path) {
            Ok(stat) => stat.rsplit(')').next().is_none_or(|tail| {
                tail.trim_start()
                    .chars()
                    .next()
                    .is_none_or(|state| matches!(state, 'Z' | 'X' | 'x'))
            }),
            Err(_error) => true,
        }
    }

    fn limits(timeout: Duration) -> Limits {
        Limits {
            stdout: Stream::Tail { cap: 4096 },
            stderr: Stream::Tail { cap: 4096 },
            quiesce_after_leader: Some(Duration::from_millis(300)),
            term_grace: Duration::from_millis(50),
            kill_settle: Duration::from_millis(500),
            reader_grace: Duration::from_millis(100),
            ..Limits::silent(timeout)
        }
    }

    #[test]
    fn deadline_kills_group_and_reaps_the_leader() {
        let dir = Scripts::new();
        let script = dir.write("deadline.sh", "sleep 30 & wait\n");
        let result = run(
            &mut command(&script),
            &limits(Duration::from_millis(250)),
            &AtomicBool::new(false),
        );
        assert!(matches!(&result.end, End::TimedOut));
        assert_eq!(result.group, GroupState::Terminated);
        assert!(matches!(result.leader, Leader::Reaped(_)));
    }

    #[test]
    fn early_leader_exit_with_background_child_is_killed_after_quiesce() {
        let dir = Scripts::new();
        let script = dir.write("quiesce.sh", "sleep 30 & exit 0\n");
        let started = Instant::now();
        let result = run(
            &mut command(&script),
            &limits(Duration::from_secs(3)),
            &AtomicBool::new(false),
        );
        assert!(matches!(&result.end, End::LeaderExited));
        assert_eq!(result.group, GroupState::Terminated);
        assert!(result.success());
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    /// A member that blocks until the test creates `release` (bounded by the test's deadlines).
    #[cfg(target_os = "linux")]
    fn lingering_member_script(
        dir: &Scripts,
        pid_file: &Path,
        release: &Path,
        code: i32,
    ) -> PathBuf {
        dir.write(
            "linger.sh",
            &format!(
                "echo $$ > '{}'\n(while [ ! -e '{}' ]; do sleep 0.02; done) >/dev/null 2>&1 &\nexit {code}\n",
                pid_file.display(),
                release.display()
            ),
        )
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn reap_then_probe_tracks_a_lingering_member_and_keeps_the_exit_code() {
        let dir = Scripts::new();
        let pid_file = dir.path().join("leader.pid");
        let release = dir.path().join("release");
        let script = lingering_member_script(&dir, &pid_file, &release, 7);
        let mut cmd = command(&script);
        cmd.process_group(0);
        let mut child = cmd.spawn().unwrap();
        let pid = child.id() as libc::pid_t;
        let mut guard = Guard {
            pid,
            active: true,
            reaped: None,
        };
        let mut liveness = LivenessScan::default();
        GLOBAL_PROC_SCANS.with(|scans| scans.set(0));
        let deadline = Instant::now() + Duration::from_secs(10);
        while guard.reaped.is_none() {
            assert!(
                own_group_live(&mut guard, &mut child, &mut liveness),
                "the lingering member keeps the group live across the reap"
            );
            assert!(Instant::now() < deadline, "the leader was never reaped");
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            own_group_live(&mut guard, &mut child, &mut liveness),
            "post-reap probe must report the lingering member"
        );
        fs::write(&release, "").unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while own_group_live(&mut guard, &mut child, &mut liveness) {
            assert!(Instant::now() < deadline, "the group never drained");
            thread::sleep(Duration::from_millis(10));
        }
        GLOBAL_PROC_SCANS.with(|scans| assert_eq!(scans.get(), 0, "reap-then-probe scanned /proc"));
        match guard.reaped.take() {
            Some(Leader::Reaped(status)) => assert_eq!(status.code(), Some(7)),
            other => panic!("leader status lost: {other:?}"),
        }
        guard.disarm();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn leader_is_reaped_while_its_group_drains_and_keeps_its_exit_code() {
        let dir = Scripts::new();
        let pid_file = dir.path().join("leader.pid");
        let release = dir.path().join("release");
        let script = lingering_member_script(&dir, &pid_file, &release, 5);
        let configured = Limits {
            quiesce_after_leader: Some(Duration::from_secs(20)),
            ..limits(Duration::from_secs(30))
        };
        let runner =
            thread::spawn(move || run(&mut command(&script), &configured, &AtomicBool::new(false)));
        let deadline = Instant::now() + Duration::from_secs(10);
        let pid = loop {
            if let Ok(value) = fs::read_to_string(&pid_file)
                && let Ok(pid) = value.trim().parse::<u32>()
            {
                break pid;
            }
            assert!(Instant::now() < deadline, "leader pid was not written");
            thread::sleep(Duration::from_millis(10));
        };
        let stat_path = PathBuf::from(format!("/proc/{pid}/stat"));
        while stat_path.exists() {
            assert!(
                Instant::now() < deadline,
                "the exited leader was not reaped while its group lingered"
            );
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            !runner.is_finished(),
            "the run must keep waiting for the lingering member"
        );
        fs::write(&release, "").unwrap();
        let result = runner.join().unwrap();
        assert!(matches!(&result.end, End::LeaderExited), "{:?}", result.end);
        assert_eq!(result.group, GroupState::Drained);
        assert!(result.leader_exited_first);
        assert!(
            matches!(&result.leader, Leader::Reaped(status) if status.code() == Some(5)),
            "{:?}",
            result.leader
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn empty_group_after_leader_exit_reads_no_proc() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "exit 3"]);
        PROC_READS.with(|reads| reads.set(0));
        GLOBAL_PROC_SCANS.with(|scans| scans.set(0));
        let result = run(
            &mut cmd,
            &limits(Duration::from_secs(10)),
            &AtomicBool::new(false),
        );
        assert!(matches!(&result.end, End::LeaderExited), "{:?}", result.end);
        assert_eq!(result.group, GroupState::Drained);
        assert!(
            matches!(&result.leader, Leader::Reaped(status) if status.code() == Some(3)),
            "{:?}",
            result.leader
        );
        PROC_READS.with(|reads| assert_eq!(reads.get(), 0, "empty group read /proc"));
        GLOBAL_PROC_SCANS.with(|scans| assert_eq!(scans.get(), 0, "empty group scanned /proc"));
    }

    #[test]
    fn empty_group_after_leader_exit_is_drained_on_every_unix_target() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "exit 3"]);
        let result = run(
            &mut cmd,
            &limits(Duration::from_secs(5)),
            &AtomicBool::new(false),
        );
        assert!(matches!(&result.end, End::LeaderExited), "{:?}", result.end);
        assert_eq!(result.group, GroupState::Drained);
        assert!(matches!(&result.leader, Leader::Reaped(status) if status.code() == Some(3)));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn darwin_group_state_ignores_zombie_leader_but_keeps_live_members() {
        assert!(!macos_process_status_is_live(libc::SZOMB));
        assert!(macos_process_status_is_live(libc::SRUN), "SRUN is live");
        assert!(macos_process_status_is_live(libc::SSLEEP), "SSLEEP is live");
        assert!(
            macos_process_status_is_live(u32::MAX),
            "unknown states fail closed"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn not_our_child_group_falls_back_to_the_proc_scan() {
        let dir = Scripts::new();
        let pid_file = dir.path().join("leader.pid");
        let release = dir.path().join("release");
        let script = lingering_member_script(&dir, &pid_file, &release, 0);
        let mut cmd = command(&script);
        cmd.process_group(0);
        let mut child = cmd.spawn().unwrap();
        let pgid = child.id();
        // The leader is reaped here, outside the supervisor: the lingering member is not our child.
        child.wait().unwrap();
        GLOBAL_PROC_SCANS.with(|scans| scans.set(0));
        let live = group_has_live_members(pgid);
        GLOBAL_PROC_SCANS
            .with(|scans| assert!(scans.get() >= 1, "the fallback did not scan /proc"));
        fs::write(&release, "").unwrap();
        assert!(live, "the lingering member must be found by the scan");
        let deadline = Instant::now() + Duration::from_secs(10);
        while group_has_live_members(pgid) {
            assert!(Instant::now() < deadline, "the group never drained");
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn post_reap_confirmation_requires_a_live_member_in_our_session() {
        let root = tempfile::tempdir().unwrap();
        let pid = 400;
        fake_proc_process(root.path(), 401, &fake_proc_stat(401, 'S', pid), "");
        fake_proc_process(root.path(), 402, &fake_proc_stat(402, 'S', 999), "");
        assert!(group_confirmed_in(root.path(), pid, 0));
        assert!(!group_confirmed_in(root.path(), pid, 9), "foreign session");
        assert!(!group_confirmed_in(root.path(), 500, 0), "no member");
        let zombies = tempfile::tempdir().unwrap();
        fake_proc_process(zombies.path(), 401, &fake_proc_stat(401, 'Z', pid), "");
        assert!(!group_confirmed_in(zombies.path(), pid, 0), "zombie member");
    }

    #[test]
    fn group_may_finish_background_work_before_the_deadline() {
        let dir = Scripts::new();
        let marker = dir.path().join("finished");
        let script = dir.write(
            "drained.sh",
            &format!("(sleep 0.3; touch '{}') & exit 0\n", marker.display()),
        );
        let configured = Limits {
            quiesce_after_leader: Some(Duration::from_secs(2)),
            ..limits(Duration::from_secs(4))
        };
        let result = run(&mut command(&script), &configured, &AtomicBool::new(false));
        assert!(matches!(&result.end, End::LeaderExited));
        assert_eq!(result.group, GroupState::Drained);
        assert!(marker.exists());
    }

    #[test]
    fn cancellation_kills_and_reaps() {
        let dir = Scripts::new();
        let script = dir.write("cancel.sh", "sleep 30 & wait\n");
        let cancel = Arc::new(AtomicBool::new(false));
        let child_cancel = Arc::clone(&cancel);
        let runner = thread::spawn(move || {
            run(
                &mut command(&script),
                &limits(Duration::from_secs(10)),
                &child_cancel,
            )
        });
        thread::sleep(Duration::from_millis(100));
        cancel.store(true, Ordering::Release);
        let result = runner.join().unwrap();
        assert!(matches!(&result.end, End::Cancelled));
        assert_eq!(result.group, GroupState::Terminated);
        assert!(matches!(result.leader, Leader::Reaped(_)));
    }

    #[test]
    fn cancel_during_quiesce_is_cancelled_with_leader_exited_first() {
        let dir = Scripts::new();
        let marker = dir.path().join("ready");
        let script = dir.write(
            "cancel-quiesce.sh",
            &format!("sleep 30 & : > '{}'; exit 0\n", marker.display()),
        );
        let cancel = Arc::new(AtomicBool::new(false));
        let child_cancel = Arc::clone(&cancel);
        let runner = thread::spawn(move || {
            let config = Limits {
                quiesce_after_leader: Some(Duration::from_secs(20)),
                ..limits(Duration::from_secs(60))
            };
            run(&mut command(&script), &config, &child_cancel)
        });
        let cancel_started = std::time::Instant::now();
        let ready_deadline = cancel_started + Duration::from_secs(30);
        while !marker.exists() {
            assert!(
                std::time::Instant::now() < ready_deadline,
                "leader never wrote its ready marker"
            );
            thread::sleep(Duration::from_millis(10));
        }
        thread::sleep(Duration::from_millis(500));
        cancel.store(true, Ordering::Release);
        let result = runner.join().unwrap();
        let elapsed = cancel_started.elapsed();
        assert!(
            matches!(&result.end, End::Cancelled),
            "end {:?} after {elapsed:?}",
            result.end
        );
        assert!(
            result.leader_exited_first,
            "leader did not exit first (waited {elapsed:?})"
        );
        assert_eq!(result.group, GroupState::Terminated, "after {elapsed:?}");
    }

    #[test]
    fn head_or_fail_overflow_is_killed() {
        let dir = Scripts::new();
        let script = dir.write("flood.sh", "yes\n");
        let configured = Limits {
            stdout: Stream::HeadOrFail { cap: 1024 },
            ..limits(Duration::from_secs(5))
        };
        let result = run(&mut command(&script), &configured, &AtomicBool::new(false));
        assert!(matches!(&result.end, End::OutputLimit(Which::Stdout)));
        assert_eq!(result.group, GroupState::Terminated);
        assert!(result.stdout.bytes.len() <= 1024);
    }

    #[test]
    fn head_burst_keeps_prefix_and_drains_to_leader_exit() {
        let dir = Scripts::new();
        let script = dir.write("head-burst.sh", "head -c 3145728 /dev/zero\n");
        let configured = Limits {
            stdout: Stream::Head { cap: 1024 * 1024 },
            stderr: Stream::Null,
            ..limits(Duration::from_secs(5))
        };
        let result = run(&mut command(&script), &configured, &AtomicBool::new(false));
        assert!(matches!(&result.end, End::LeaderExited));
        assert_eq!(result.stdout.bytes, vec![0; 1024 * 1024]);
        assert!(result.stdout.truncated);
        assert_eq!(result.stdout.total, 3 * 1024 * 1024);
    }

    #[test]
    fn missing_program_is_spawn_not_found() {
        let mut cmd = Command::new("/no/such/thegn-proc-program");
        let result = run(
            &mut cmd,
            &Limits::silent(Duration::from_secs(1)),
            &AtomicBool::new(false),
        );
        assert!(
            matches!(&result.end, End::Spawn(error) if error.kind() == io::ErrorKind::NotFound)
        );
        assert_eq!(result.group, GroupState::Unknown);
    }

    #[test]
    fn tail_flood_past_cap_does_not_block() {
        let dir = Scripts::new();
        let script = dir.write(
            "tail.sh",
            "head -c 2097152 /dev/zero; head -c 2097152 /dev/zero >&2\n",
        );
        let configured = Limits {
            stdout: Stream::Tail { cap: 32 * 1024 },
            stderr: Stream::Tail { cap: 32 * 1024 },
            aggregate_tail_cap: Some(48 * 1024),
            ..limits(Duration::from_secs(10))
        };
        let result = run(&mut command(&script), &configured, &AtomicBool::new(false));
        assert!(matches!(&result.end, End::LeaderExited));
        assert_eq!(
            result.stdout.bytes.len() + result.stderr.bytes.len(),
            48 * 1024
        );
        assert!(result.stdout.truncated || result.stderr.truncated);
        assert!(result.stdout.total >= 2 * 1024 * 1024);
        assert!(result.stderr.total >= 2 * 1024 * 1024);
    }

    #[test]
    fn zero_reader_grace_drains_large_final_burst_to_eof() {
        let dir = Scripts::new();
        let script = dir.write("burst.sh", "head -c 204800 /dev/zero\n");
        let configured = Limits {
            stdout: Stream::Tail { cap: 1024 * 1024 },
            stderr: Stream::Null,
            reader_grace: Duration::ZERO,
            ..limits(Duration::from_secs(3))
        };
        let result = run(&mut command(&script), &configured, &AtomicBool::new(false));
        assert!(matches!(&result.end, End::LeaderExited));
        assert_eq!(result.stdout.total, 200 * 1024);
        assert_eq!(result.stdout.bytes.len(), 200 * 1024);
        assert!(result.stdout.eof);
    }

    #[test]
    fn term_resistant_descendant_is_killed() {
        if !Path::new("/proc/self/stat").exists() {
            return;
        }
        let dir = Scripts::new();
        let pid_file = dir.path().join("child.pid");
        let script = dir.write(
            "resist.sh",
            &format!(
                "trap '' TERM; sleep 30 & child=$!; echo $child > '{}'; wait\n",
                pid_file.display()
            ),
        );
        let result = run(
            &mut command(&script),
            &limits(Duration::from_millis(250)),
            &AtomicBool::new(false),
        );
        assert_eq!(result.group, GroupState::Terminated);
        let pid: u32 = fs::read_to_string(pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(proc_state_gone_or_zombie(pid));
    }

    #[test]
    fn escaped_descendant_holding_the_pipe_is_bounded_by_reader_grace() {
        let mut probe = Command::new("setsid");
        probe.arg("true");
        if !run(
            &mut probe,
            &Limits::silent(Duration::from_secs(1)),
            &AtomicBool::new(false),
        )
        .success()
        {
            return;
        }
        let dir = Scripts::new();
        let script = dir.write("escape.sh", "setsid sleep 30 & exit 0\n");
        let configured = Limits {
            stdout: Stream::Tail { cap: 128 },
            stderr: Stream::Null,
            quiesce_after_leader: Some(Duration::from_millis(100)),
            reader_grace: Duration::from_millis(150),
            ..Limits::silent(Duration::from_secs(2))
        };
        let started = Instant::now();
        let result = run(&mut command(&script), &configured, &AtomicBool::new(false));
        assert!(started.elapsed() < Duration::from_secs(3));
        assert!(!result.stdout.eof);
    }

    #[test]
    fn timed_out_run_does_not_prevent_the_next() {
        let dir = Scripts::new();
        let slow = dir.write("slow.sh", "sleep 30\n");
        let fast = dir.write("fast.sh", "printf done\n");
        let first = run(
            &mut command(&slow),
            &limits(Duration::from_millis(100)),
            &AtomicBool::new(false),
        );
        assert!(matches!(&first.end, End::TimedOut));
        let second = run(
            &mut command(&fast),
            &limits(Duration::from_secs(2)),
            &AtomicBool::new(false),
        );
        assert!(second.success());
        assert_eq!(second.stdout.bytes, b"done");
    }

    #[test]
    fn next_backoff_doubles_caps_and_never_overshoots() {
        let sequence: Vec<_> = (0..7)
            .scan(POLL_INTERVAL, |current, _| {
                let value = *current;
                *current = crate::next_backoff(*current, BACKOFF_CAP);
                Some(value.as_millis())
            })
            .collect();
        assert_eq!(sequence, vec![10, 20, 40, 80, 160, 250, 250]);
        assert_eq!(
            crate::next_backoff(Duration::from_millis(20), Duration::from_millis(7)),
            Duration::from_millis(7)
        );
    }

    #[test]
    fn aggregate_fit_trims_the_larger_tail_first() {
        let mut stdout = Capture {
            bytes: vec![1; 8],
            ..Capture::default()
        };
        let mut stderr = Capture {
            bytes: vec![2; 4],
            ..Capture::default()
        };
        aggregate_fit(
            &mut stdout,
            &mut stderr,
            Stream::Tail { cap: 8 },
            Stream::Tail { cap: 4 },
            Some(8),
        );
        assert_eq!(stdout.bytes.len(), 4);
        assert_eq!(stderr.bytes.len(), 4);
        assert!(stdout.truncated);
        assert!(!stderr.truncated);
    }

    #[test]
    fn aggregate_fit_preserves_head_and_trims_only_tail() {
        let mut stdout = Capture {
            bytes: vec![1; 8],
            ..Capture::default()
        };
        let mut stderr = Capture {
            bytes: vec![2; 4],
            ..Capture::default()
        };
        aggregate_fit(
            &mut stdout,
            &mut stderr,
            Stream::Head { cap: 8 },
            Stream::Tail { cap: 4 },
            Some(8),
        );
        assert_eq!(stdout.bytes, vec![1; 8]);
        assert!(stderr.bytes.is_empty());
        assert!(stderr.truncated);
        assert!(!stdout.truncated);
    }

    #[test]
    fn limits_silent_defaults() {
        let configured = Limits::silent(Duration::from_secs(4));
        assert_eq!(configured.timeout, Duration::from_secs(4));
        assert_eq!(configured.quiesce_after_leader, None);
        assert_eq!(configured.leader_exit, LeaderExit::Quiesce);
        assert_eq!(configured.term_grace, Duration::ZERO);
        assert_eq!(configured.kill_settle, Duration::from_millis(250));
        assert_eq!(configured.reader_grace, Duration::ZERO);
        assert_eq!(configured.stdout, Stream::Null);
        assert_eq!(configured.stderr, Stream::Null);
        assert_eq!(configured.aggregate_tail_cap, None);
    }

    fn detach_limits(timeout: Duration) -> Limits {
        Limits {
            stdout: Stream::Tail { cap: 4096 },
            stderr: Stream::Tail { cap: 4096 },
            leader_exit: LeaderExit::Detach,
            term_grace: Duration::from_millis(50),
            kill_settle: Duration::from_millis(500),
            reader_grace: Duration::from_millis(200),
            ..Limits::silent(timeout)
        }
    }

    #[test]
    fn detach_leaves_background_member_running_and_returns_promptly() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "sleep 30 & echo \"started $$\""]);
        let started = Instant::now();
        let run = run(
            &mut cmd,
            &detach_limits(Duration::from_secs(10)),
            &AtomicBool::new(false),
        );
        let elapsed = started.elapsed();
        let text = String::from_utf8_lossy(&run.stdout.bytes).into_owned();
        let pgid: libc::pid_t = text
            .trim()
            .strip_prefix("started ")
            .and_then(|pid| pid.parse().ok())
            .unwrap_or_else(|| panic!("no pid in {text:?}"));
        let survivors = crate::group_has_live_members(pgid as u32);
        // SAFETY: killpg only signals the test's own detached process group.
        let killed = unsafe { libc::killpg(pgid, libc::SIGKILL) };
        assert!(elapsed < Duration::from_secs(3), "{elapsed:?}");
        assert!(matches!(&run.end, End::LeaderExited), "{:?}", run.end);
        assert!(
            matches!(&run.leader, Leader::Reaped(status) if status.success()),
            "{:?}",
            run.leader
        );
        assert_eq!(run.group, GroupState::Detached);
        assert!(run.leader_exited_first);
        assert!(run.success());
        assert_eq!(run.error(), None);
        assert!(text.contains("started"), "{text:?}");
        assert!(survivors, "the backgrounded sleep must still be running");
        assert_eq!(
            killed, 0,
            "the detached group must still exist to be killed"
        );
    }

    #[test]
    fn detach_with_no_survivors_reports_drained() {
        let run = run(
            &mut Command::new("true"),
            &detach_limits(Duration::from_secs(10)),
            &AtomicBool::new(false),
        );
        assert!(matches!(&run.end, End::LeaderExited), "{:?}", run.end);
        assert_eq!(run.group, GroupState::Drained);
        assert!(run.success());
        assert!(run.stdout.eof);
    }

    #[test]
    fn detach_survivor_keeps_writing_after_reader_grace() {
        let scripts = Scripts::new();
        let marker = scripts.path().join("alive");
        let mut cmd = Command::new("sh");
        cmd.args([
            "-c",
            "(sleep 1; echo late; echo late >&2; : > \"$1\") & echo started",
            "sh",
        ])
        .arg(&marker);
        let run = run(
            &mut cmd,
            &detach_limits(Duration::from_secs(10)),
            &AtomicBool::new(false),
        );
        assert_eq!(run.group, GroupState::Detached);
        let deadline = Instant::now() + Duration::from_secs(20);
        while !marker.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        assert!(
            marker.exists(),
            "the survivor died writing to an abandoned pipe"
        );
    }

    #[test]
    fn detach_still_terminates_on_timeout() {
        let scripts = Scripts::new();
        let script = scripts.write("detach-timeout.sh", "sleep 30\n");
        let started = Instant::now();
        let run = run(
            &mut command(&script),
            &detach_limits(Duration::from_millis(200)),
            &AtomicBool::new(false),
        );
        assert!(matches!(&run.end, End::TimedOut), "{:?}", run.end);
        assert_eq!(run.group, GroupState::Terminated);
        assert!(matches!(run.leader, Leader::Reaped(_)), "{:?}", run.leader);
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn detach_still_terminates_on_cancel() {
        let scripts = Scripts::new();
        let script = scripts.write("detach-cancel.sh", "sleep 30 & wait\n");
        let cancel = Arc::new(AtomicBool::new(false));
        let child_cancel = Arc::clone(&cancel);
        let started = Instant::now();
        let runner = thread::spawn(move || {
            run(
                &mut command(&script),
                &detach_limits(Duration::from_secs(10)),
                &child_cancel,
            )
        });
        thread::sleep(Duration::from_millis(100));
        cancel.store(true, Ordering::Release);
        let run = runner.join().unwrap();
        assert!(matches!(&run.end, End::Cancelled), "{:?}", run.end);
        assert_eq!(run.group, GroupState::Terminated);
        assert!(!run.leader_exited_first);
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn consume_stream_hands_every_byte_in_order_and_retains_none() {
        let scripts = Scripts::new();
        let script = scripts.write("consume.sh", "yes x | head -c 3000000; printf 'tail\\n'\n");
        let limits = Limits {
            stdout: Stream::Consume,
            stderr: Stream::Null,
            ..Limits::silent(Duration::from_secs(20))
        };
        let (mut total, mut last, mut first_mismatch) = (0_u64, Vec::new(), None);
        let run = run_consuming(
            &mut command(&script),
            &limits,
            &AtomicBool::new(false),
            &mut |bytes: &[u8]| {
                for &byte in bytes {
                    let expected = if total < 3_000_000 {
                        Some(if total.is_multiple_of(2) { b'x' } else { b'\n' })
                    } else {
                        b"tail\n".get((total - 3_000_000) as usize).copied()
                    };
                    if expected != Some(byte) && first_mismatch.is_none() {
                        first_mismatch = Some(total);
                    }
                    total += 1;
                    if last.len() == 5 {
                        last.remove(0);
                    }
                    last.push(byte);
                }
            },
        );
        assert!(run.success());
        assert_eq!(first_mismatch, None, "first mismatched byte index");
        assert_eq!(total, 3_000_005);
        assert_eq!(last, b"tail\n");
        assert!(run.stdout.bytes.is_empty());
        assert_eq!(run.stdout.total, 3_000_005);
        assert!(run.stdout.eof);
    }

    #[test]
    fn consume_stream_keeps_the_deadline_and_kills_the_group() {
        let scripts = Scripts::new();
        let script = scripts.write("consume-timeout.sh", "printf x; sleep 30\n");
        let limits = Limits {
            stdout: Stream::Consume,
            stderr: Stream::Null,
            ..Limits::silent(Duration::from_millis(300))
        };
        let mut seen = Vec::new();
        let started = Instant::now();
        let run = run_consuming(
            &mut command(&script),
            &limits,
            &AtomicBool::new(false),
            &mut |bytes: &[u8]| seen.extend_from_slice(bytes),
        );
        assert!(matches!(run.end, End::TimedOut));
        assert_eq!(seen, b"x");
        assert!(matches!(run.leader, Leader::Reaped(_)));
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn plain_run_with_consume_stream_drains_without_retention() {
        let scripts = Scripts::new();
        let script = scripts.write("consume-plain.sh", "head -c 100000 /dev/zero\n");
        let limits = Limits {
            stdout: Stream::Consume,
            stderr: Stream::Null,
            ..Limits::silent(Duration::from_secs(10))
        };
        let run = run(&mut command(&script), &limits, &AtomicBool::new(false));
        assert!(run.success());
        assert!(run.stdout.bytes.is_empty());
        assert_eq!(run.stdout.total, 100_000);
    }

    #[test]
    fn consume_streams_hand_both_streams_every_byte_in_order_and_retain_none() {
        let scripts = Scripts::new();
        let script = scripts.write(
            "consume-both.sh",
            "(i=0; while [ \"$i\" -lt 2000 ]; do printf 'e%04d' \"$i\" >&2; i=$((i+1)); done) &\n\
             yes x | head -c 3000000\nwait\nprintf 'tail\\n'\n",
        );
        let limits = Limits {
            stdout: Stream::Consume,
            stderr: Stream::Consume,
            ..Limits::silent(Duration::from_secs(20))
        };
        let (mut total, mut last, mut first_mismatch) = (0_u64, Vec::new(), None);
        let mut err_seen = Vec::new();
        let run = run_consuming_streams(
            &mut command(&script),
            &limits,
            &AtomicBool::new(false),
            &mut |which: Which, bytes: &[u8]| match which {
                Which::Stdout => {
                    for &byte in bytes {
                        let expected = if total < 3_000_000 {
                            Some(if total.is_multiple_of(2) { b'x' } else { b'\n' })
                        } else {
                            b"tail\n".get((total - 3_000_000) as usize).copied()
                        };
                        if expected != Some(byte) && first_mismatch.is_none() {
                            first_mismatch = Some(total);
                        }
                        total += 1;
                        if last.len() == 5 {
                            last.remove(0);
                        }
                        last.push(byte);
                    }
                }
                Which::Stderr => err_seen.extend_from_slice(bytes),
            },
        );
        let expected_err = (0..2000)
            .map(|index| format!("e{index:04}"))
            .collect::<String>()
            .into_bytes();
        assert!(run.success());
        assert_eq!(first_mismatch, None, "first mismatched stdout byte index");
        assert_eq!(total, 3_000_005);
        assert_eq!(last, b"tail\n");
        assert_eq!(err_seen, expected_err);
        assert!(run.stdout.bytes.is_empty());
        assert!(run.stderr.bytes.is_empty());
        assert_eq!(run.stdout.total, 3_000_005);
        assert_eq!(run.stderr.total, 10_000);
        assert!(run.stdout.eof);
        assert!(run.stderr.eof);
    }

    #[test]
    fn consume_streams_deliver_stderr_while_stdout_head_is_retained() {
        let scripts = Scripts::new();
        let script = scripts.write(
            "consume-stderr-only.sh",
            "head -c 5000 /dev/zero\nprintf 'warn' >&2\n",
        );
        let limits = Limits {
            stdout: Stream::Head { cap: 1000 },
            stderr: Stream::Consume,
            ..Limits::silent(Duration::from_secs(10))
        };
        let (mut stdout_calls, mut err_seen) = (0_usize, Vec::new());
        let run = run_consuming_streams(
            &mut command(&script),
            &limits,
            &AtomicBool::new(false),
            &mut |which: Which, bytes: &[u8]| match which {
                Which::Stdout => stdout_calls += 1,
                Which::Stderr => err_seen.extend_from_slice(bytes),
            },
        );
        assert!(run.success());
        assert_eq!(stdout_calls, 0);
        assert_eq!(err_seen, b"warn");
        assert_eq!(run.stdout.bytes, vec![0_u8; 1000]);
        assert!(run.stdout.truncated);
        assert_eq!(run.stdout.total, 5000);
        assert!(run.stderr.bytes.is_empty());
        assert_eq!(run.stderr.total, 4);
    }

    #[test]
    fn run_consuming_hands_only_stdout_even_with_stderr_consume() {
        let scripts = Scripts::new();
        let script = scripts.write("consume-wrapper.sh", "printf out\nprintf err >&2\n");
        let limits = Limits {
            stdout: Stream::Consume,
            stderr: Stream::Consume,
            ..Limits::silent(Duration::from_secs(10))
        };
        let mut seen = Vec::new();
        let run = run_consuming(
            &mut command(&script),
            &limits,
            &AtomicBool::new(false),
            &mut |bytes: &[u8]| seen.extend_from_slice(bytes),
        );
        assert!(run.success());
        assert_eq!(seen, b"out");
        assert!(run.stderr.bytes.is_empty());
        assert_eq!(run.stderr.total, 3);
        assert!(run.stderr.eof);
    }

    #[test]
    fn stderr_consume_keeps_the_deadline_and_kills_the_group() {
        let scripts = Scripts::new();
        let script = scripts.write("consume-stderr-timeout.sh", "printf x >&2; sleep 30\n");
        let limits = Limits {
            stdout: Stream::Null,
            stderr: Stream::Consume,
            ..Limits::silent(Duration::from_millis(300))
        };
        let mut seen = Vec::new();
        let started = Instant::now();
        let run = run_consuming_streams(
            &mut command(&script),
            &limits,
            &AtomicBool::new(false),
            &mut |which: Which, bytes: &[u8]| {
                if which == Which::Stderr {
                    seen.extend_from_slice(bytes);
                }
            },
        );
        assert!(matches!(run.end, End::TimedOut));
        assert_eq!(seen, b"x");
        assert!(matches!(run.leader, Leader::Reaped(_)));
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn stderr_consumer_panic_unwinds_after_killing_and_reaping_the_group() {
        let scripts = Scripts::new();
        let pid_file = scripts.path().join("leader.pid");
        let script = scripts.write(
            "consume-stderr-panic.sh",
            &format!(
                "printf '%s' \"$$\" > '{}'\nprintf boom >&2\nsleep 30\n",
                pid_file.display()
            ),
        );
        let limits = Limits {
            stdout: Stream::Null,
            stderr: Stream::Consume,
            ..Limits::silent(Duration::from_secs(20))
        };
        let started = Instant::now();
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_consuming_streams(
                &mut command(&script),
                &limits,
                &AtomicBool::new(false),
                &mut |which: Which, _bytes: &[u8]| {
                    if which == Which::Stderr {
                        panic!("consumer rejects stderr");
                    }
                },
            )
        }));
        assert!(outcome.is_err(), "the consumer panic must propagate");
        assert!(started.elapsed() < Duration::from_secs(5));
        let pid: u32 = fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(proc_state_gone_or_zombie(pid));
        let settle = Instant::now() + Duration::from_secs(2);
        while group_live(pid as libc::pid_t) && Instant::now() < settle {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            !group_live(pid as libc::pid_t),
            "the guard must kill the group"
        );
    }

    #[test]
    fn proc_error_classifies_each_run_end() {
        let scripts = Scripts::new();
        let successful = scripts.write("proc-error-success.sh", "exit 0\n");
        let result = run(
            &mut command(&successful),
            &Limits::silent(Duration::from_secs(2)),
            &AtomicBool::new(false),
        );
        assert_eq!(result.error(), None);
        assert_eq!(result.error().is_none(), result.success());

        let nonzero = scripts.write("proc-error-nonzero.sh", "exit 3\n");
        let result = run(
            &mut command(&nonzero),
            &Limits::silent(Duration::from_secs(2)),
            &AtomicBool::new(false),
        );
        assert!(
            matches!(result.error(), Some(crate::ProcError::NonZero(status)) if status.code() == Some(3))
        );
        assert_eq!(result.error().is_none(), result.success());

        let timeout = scripts.write("proc-error-timeout.sh", "sleep 30 & wait\n");
        let result = run(
            &mut command(&timeout),
            &limits(Duration::from_millis(250)),
            &AtomicBool::new(false),
        );
        assert!(matches!(result.error(), Some(crate::ProcError::TimedOut)));
        assert_eq!(result.error().is_none(), result.success());

        let overflow = scripts.write("proc-error-overflow.sh", "yes\n");
        let output_limits = Limits {
            stdout: Stream::HeadOrFail { cap: 1024 },
            ..Limits::silent(Duration::from_secs(2))
        };
        let result = run(
            &mut command(&overflow),
            &output_limits,
            &AtomicBool::new(false),
        );
        assert!(matches!(
            result.error(),
            Some(crate::ProcError::OutputLimit(Which::Stdout))
        ));
        assert_eq!(result.error().is_none(), result.success());

        let mut missing = Command::new("/no/such/thegn-proc-program");
        let result = run(
            &mut missing,
            &Limits::silent(Duration::from_secs(1)),
            &AtomicBool::new(false),
        );
        assert!(matches!(
            result.error(),
            Some(crate::ProcError::Spawn {
                kind: io::ErrorKind::NotFound,
                ..
            })
        ));
        assert_eq!(result.error().is_none(), result.success());
    }

    #[test]
    fn proc_error_reports_cancellation() {
        let scripts = Scripts::new();
        let script = scripts.write("proc-error-cancel.sh", "sleep 30 & wait\n");
        let cancel = Arc::new(AtomicBool::new(false));
        let child_cancel = Arc::clone(&cancel);
        let runner = thread::spawn(move || {
            run(
                &mut command(&script),
                &limits(Duration::from_secs(10)),
                &child_cancel,
            )
        });
        thread::sleep(Duration::from_millis(100));
        cancel.store(true, Ordering::Release);
        let result = runner.join().unwrap();
        assert_eq!(result.error(), Some(crate::ProcError::Cancelled));
        assert_eq!(result.error().is_none(), result.success());
    }

    #[test]
    fn proc_error_display_names_each_class() {
        assert_eq!(crate::ProcError::TimedOut.to_string(), "timed out");
        assert_eq!(crate::ProcError::Cancelled.to_string(), "cancelled");
        assert_eq!(
            crate::ProcError::OutputLimit(Which::Stderr).to_string(),
            "stderr exceeded its byte cap"
        );
        assert_eq!(
            crate::ProcError::Spawn {
                kind: io::ErrorKind::NotFound,
                message: "boom".into(),
            }
            .to_string(),
            "could not run: boom"
        );
        assert_eq!(
            crate::ProcError::Unreaped.to_string(),
            "was not reaped within the kill budget"
        );
        let scripts = Scripts::new();
        let nonzero = scripts.write("proc-error-display.sh", "exit 3\n");
        let result = run(
            &mut command(&nonzero),
            &Limits::silent(Duration::from_secs(2)),
            &AtomicBool::new(false),
        );
        let Some(crate::ProcError::NonZero(status)) = result.error() else {
            panic!("exit 3 must produce a typed nonzero error");
        };
        assert_eq!(
            crate::ProcError::NonZero(status).to_string(),
            format!("exited with {status}")
        );
    }

    #[test]
    fn input_payload_reaches_the_child_and_closes_stdin() {
        let scripts = Scripts::new();
        let script = scripts.write("input-round-trip.sh", "cat\n");
        let input = b"alpha\nbeta\n";
        let (run, feed) = crate::run_with_input(
            &mut command(&script),
            &limits(Duration::from_secs(5)),
            input,
            &AtomicBool::new(false),
        );
        assert!(run.success());
        assert_eq!(run.stdout.bytes, input);
        assert_eq!(
            feed,
            crate::InputFeed {
                written: 11,
                complete: true,
                error: None,
            }
        );
    }

    #[test]
    fn large_input_feeds_without_deadlock_while_output_drains() {
        let scripts = Scripts::new();
        let script = scripts.write("input-large.sh", "cat\n");
        let input = vec![b'x'; 1 << 20];
        let (run, feed) = crate::run_with_input(
            &mut command(&script),
            &limits(Duration::from_secs(10)),
            &input,
            &AtomicBool::new(false),
        );
        assert!(run.success());
        assert!(feed.complete);
        assert_eq!(feed.written, 1 << 20);
        assert_eq!(run.stdout.total, 1 << 20);
    }

    #[test]
    fn child_that_never_reads_stdin_does_not_block_the_run() {
        let scripts = Scripts::new();
        let script = scripts.write("input-unread.sh", "exit 0\n");
        let input = vec![0_u8; 1 << 20];
        let started = Instant::now();
        let (run, feed) = crate::run_with_input(
            &mut command(&script),
            &limits(Duration::from_secs(10)),
            &input,
            &AtomicBool::new(false),
        );
        assert!(matches!(run.end, End::LeaderExited));
        assert!(!feed.complete);
        assert!(feed.written < (1 << 20));
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn stdin_holding_child_still_times_out_and_is_killed() {
        let scripts = Scripts::new();
        let script = scripts.write("input-held.sh", "sleep 30\n");
        let input = vec![0_u8; 1 << 20];
        let (run, feed) = crate::run_with_input(
            &mut command(&script),
            &limits(Duration::from_millis(300)),
            &input,
            &AtomicBool::new(false),
        );
        assert!(matches!(run.end, End::TimedOut));
        assert_eq!(run.group, GroupState::Terminated);
        assert!(matches!(run.leader, Leader::Reaped(_)));
        assert!(!feed.complete);
    }

    /// A release sender, and the holder reporting (released before its 30 s cap, late write).
    type StdinHolder = (mpsc::Sender<()>, thread::JoinHandle<(bool, io::Result<()>)>);

    /// Moves a handed-off stdin into a thread that holds it until released, then writes once more
    /// and reports that write's result.
    fn hold_stdin(stdin: ChildStdin) -> StdinHolder {
        let (release, released) = mpsc::channel();
        let holder = thread::spawn(move || {
            let mut stdin = stdin;
            let on_time = released.recv_timeout(Duration::from_secs(30)).is_ok();
            let write = stdin.write_all(b"late\n");
            (on_time, write)
        });
        (release, holder)
    }

    #[test]
    fn stdin_handoff_writer_reaches_cat_and_the_run_succeeds() {
        let scripts = Scripts::new();
        let script = scripts.write("handoff-cat.sh", "cat\n");
        let mut writer = None;
        let mut handoffs = 0_u32;
        let mut seen = Vec::new();
        let run = crate::run_consuming_streams_with_stdin(
            &mut command(&script),
            &Limits {
                stdout: Stream::Consume,
                ..limits(Duration::from_secs(5))
            },
            &AtomicBool::new(false),
            &mut |mut stdin: ChildStdin| {
                handoffs = handoffs.saturating_add(1);
                writer = Some(thread::spawn(move || {
                    let written = stdin.write_all(b"hello\n");
                    drop(stdin);
                    written
                }));
            },
            &mut |which: Which, bytes: &[u8]| {
                if which == Which::Stdout {
                    seen.extend_from_slice(bytes);
                }
            },
        );
        assert!(run.success(), "{:?}", run.end);
        assert_eq!(handoffs, 1);
        assert_eq!(seen, b"hello\n");
        assert!(run.stdout.bytes.is_empty(), "Consume retains nothing");
        writer
            .expect("on_stdin was called")
            .join()
            .unwrap()
            .unwrap();
    }

    #[test]
    fn stdin_held_open_by_the_caller_still_times_out_killed_and_reaped() {
        let scripts = Scripts::new();
        // cat never sees EOF while the caller holds stdin, so only the deadline can end it.
        let script = scripts.write("handoff-held.sh", "cat\n");
        let configured = limits(Duration::from_millis(300));
        let bound = configured.timeout
            + configured.term_grace
            + configured.kill_settle * 2
            + configured.reader_grace
            + BACKOFF_CAP;
        let mut held = None;
        let started = Instant::now();
        let run = crate::run_consuming_streams_with_stdin(
            &mut command(&script),
            &configured,
            &AtomicBool::new(false),
            &mut |stdin: ChildStdin| held = Some(hold_stdin(stdin)),
            &mut |_which: Which, _bytes: &[u8]| {},
        );
        let elapsed = started.elapsed();
        assert!(matches!(&run.end, End::TimedOut), "{:?}", run.end);
        assert_eq!(run.error(), Some(crate::ProcError::TimedOut));
        assert_eq!(run.group, GroupState::Terminated);
        assert!(matches!(run.leader, Leader::Reaped(_)));
        assert!(
            elapsed <= bound + Duration::from_secs(3),
            "{elapsed:?} > documented bound {bound:?} + 3s scheduling slack"
        );
        let (release, holder) = held.expect("on_stdin was called");
        release.send(()).unwrap();
        let (on_time, late_write) = holder.join().unwrap();
        assert!(on_time, "the caller still held stdin when the run ended");
        // The group is gone, so the caller's writer sees EPIPE rather than a signal.
        assert_eq!(late_write.unwrap_err().kind(), io::ErrorKind::BrokenPipe);
    }

    #[test]
    fn cancel_while_the_caller_holds_stdin_ends_and_reaps_the_run() {
        let scripts = Scripts::new();
        let script = scripts.write("handoff-cancel.sh", "cat\n");
        let cancel = Arc::new(AtomicBool::new(false));
        let canceller_flag = Arc::clone(&cancel);
        let canceller = thread::spawn(move || {
            thread::sleep(Duration::from_millis(100));
            canceller_flag.store(true, Ordering::Release);
        });
        let mut held = None;
        let started = Instant::now();
        let run = crate::run_consuming_streams_with_stdin(
            &mut command(&script),
            &limits(Duration::from_secs(10)),
            &cancel,
            &mut |stdin: ChildStdin| held = Some(hold_stdin(stdin)),
            &mut |_which: Which, _bytes: &[u8]| {},
        );
        canceller.join().unwrap();
        assert!(matches!(&run.end, End::Cancelled), "{:?}", run.end);
        assert_eq!(run.error(), Some(crate::ProcError::Cancelled));
        assert_eq!(run.group, GroupState::Terminated);
        assert!(matches!(run.leader, Leader::Reaped(_)));
        assert!(started.elapsed() < Duration::from_secs(5));
        let (release, holder) = held.expect("on_stdin was called");
        release.send(()).unwrap();
        let (on_time, _late_write) = holder.join().unwrap();
        assert!(on_time);
    }

    #[test]
    fn child_closing_stdin_early_gives_the_writer_broken_pipe_and_still_succeeds() {
        let scripts = Scripts::new();
        let script = scripts.write("handoff-closed.sh", "exec 0<&-; sleep 0.1\n");
        let mut writer = None;
        let run = crate::run_consuming_streams_with_stdin(
            &mut command(&script),
            &limits(Duration::from_secs(5)),
            &AtomicBool::new(false),
            &mut |mut stdin: ChildStdin| {
                writer = Some(thread::spawn(move || {
                    let chunk = [b'x'; 4096];
                    // Writes succeed into the pipe buffer until the child's close; a blocked
                    // writer is woken with EPIPE once the last reader is gone.
                    loop {
                        if let Err(error) = stdin.write_all(&chunk) {
                            return error.kind();
                        }
                    }
                }));
            },
            &mut |_which: Which, _bytes: &[u8]| {},
        );
        assert!(run.success(), "{:?}", run.end);
        let kind = writer.expect("on_stdin was called").join().unwrap();
        assert_eq!(kind, io::ErrorKind::BrokenPipe);
    }

    #[test]
    fn stdin_handoff_spawn_failure_skips_the_handoff_and_matches_run() {
        let program = "/no/such/thegn-proc-program";
        let mut called = false;
        let handoff_run = crate::run_consuming_streams_with_stdin(
            &mut Command::new(program),
            &Limits::silent(Duration::from_secs(1)),
            &AtomicBool::new(false),
            &mut |_stdin: ChildStdin| called = true,
            &mut |_which: Which, _bytes: &[u8]| {},
        );
        let plain = run(
            &mut Command::new(program),
            &Limits::silent(Duration::from_secs(1)),
            &AtomicBool::new(false),
        );
        assert!(!called, "on_stdin must not run when spawn fails");
        assert_eq!(handoff_run.error(), plain.error());
        assert!(matches!(
            handoff_run.error(),
            Some(crate::ProcError::Spawn {
                kind: io::ErrorKind::NotFound,
                ..
            })
        ));
        assert_eq!(handoff_run.group, GroupState::Unknown);
        assert!(matches!(handoff_run.leader, Leader::NotSpawned));
    }

    #[test]
    fn empty_input_delivers_eof_at_once() {
        let scripts = Scripts::new();
        let script = scripts.write("input-empty.sh", "cat\n");
        let (run, feed) = crate::run_with_input(
            &mut command(&script),
            &limits(Duration::from_secs(5)),
            b"",
            &AtomicBool::new(false),
        );
        assert!(run.success());
        assert!(run.stdout.bytes.is_empty());
        assert_eq!(
            feed,
            crate::InputFeed {
                written: 0,
                complete: true,
                error: None,
            }
        );
    }

    #[test]
    fn plain_run_keeps_stdin_null() {
        // Positive control: this already passes without run_with_input.
        let scripts = Scripts::new();
        let script = scripts.write("input-null.sh", "cat\n");
        let run = run(
            &mut command(&script),
            &limits(Duration::from_secs(5)),
            &AtomicBool::new(false),
        );
        assert!(run.success());
        assert!(run.stdout.bytes.is_empty());
    }

    fn idle_limits(timeout: Duration, idle: Option<Duration>) -> Limits {
        Limits {
            idle_output: idle,
            ..limits(timeout)
        }
    }

    #[test]
    fn idle_output_ends_a_silent_child_long_before_the_timeout() {
        let scripts = Scripts::new();
        let script = scripts.write("idle-silent.sh", "printf x\nsleep 30\n");
        let started = Instant::now();
        let run = run(
            &mut command(&script),
            &idle_limits(Duration::from_secs(20), Some(Duration::from_millis(300))),
            &AtomicBool::new(false),
        );
        assert!(matches!(&run.end, End::TimedOut), "{:?}", run.end);
        assert!(run.idle_expired);
        assert_eq!(run.group, GroupState::Terminated);
        assert!(matches!(run.leader, Leader::Reaped(_)));
        assert_eq!(run.stdout.bytes, b"x");
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn idle_output_expires_a_child_with_no_pipes_that_never_writes() {
        let scripts = Scripts::new();
        let script = scripts.write("idle-null.sh", "sleep 30\n");
        let limits = Limits {
            idle_output: Some(Duration::from_millis(300)),
            ..Limits::silent(Duration::from_secs(20))
        };
        let started = Instant::now();
        let run = run(&mut command(&script), &limits, &AtomicBool::new(false));
        assert!(matches!(&run.end, End::TimedOut), "{:?}", run.end);
        assert!(run.idle_expired);
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn steady_stdout_resets_the_idle_clock() {
        let scripts = Scripts::new();
        // 20 writes 100 ms apart: about 2 s of output, twice the idle bound.
        let script = scripts.write(
            "idle-steady.sh",
            "i=0\nwhile [ $i -lt 20 ]; do printf x; sleep 0.1; i=$((i+1)); done\n",
        );
        let started = Instant::now();
        let run = run(
            &mut command(&script),
            &idle_limits(Duration::from_secs(20), Some(Duration::from_secs(1))),
            &AtomicBool::new(false),
        );
        assert!(matches!(&run.end, End::LeaderExited), "{:?}", run.end);
        assert!(!run.idle_expired);
        assert!(run.success());
        assert_eq!(run.stdout.total, 20);
        assert!(started.elapsed() > Duration::from_secs(1));
    }

    #[test]
    fn stderr_only_consume_output_resets_the_idle_clock() {
        let scripts = Scripts::new();
        let script = scripts.write(
            "idle-stderr.sh",
            "i=0\nwhile [ $i -lt 20 ]; do printf e >&2; sleep 0.1; i=$((i+1)); done\n",
        );
        let limits = Limits {
            stdout: Stream::Tail { cap: 64 },
            stderr: Stream::Consume,
            idle_output: Some(Duration::from_secs(1)),
            ..Limits::silent(Duration::from_secs(20))
        };
        let mut seen = Vec::new();
        let run = run_consuming_streams(
            &mut command(&script),
            &limits,
            &AtomicBool::new(false),
            &mut |which: Which, bytes: &[u8]| {
                if which == Which::Stderr {
                    seen.extend_from_slice(bytes);
                }
            },
        );
        assert!(matches!(&run.end, End::LeaderExited), "{:?}", run.end);
        assert!(!run.idle_expired);
        assert!(run.success());
        assert_eq!(seen.len(), 20);
        assert_eq!(run.stdout.total, 0);
    }

    #[test]
    fn no_idle_bound_lets_a_quiet_child_finish() {
        let scripts = Scripts::new();
        let script = scripts.write("idle-none.sh", "printf a\nsleep 1\nprintf b\n");
        let run = run(
            &mut command(&script),
            &idle_limits(Duration::from_secs(20), None),
            &AtomicBool::new(false),
        );
        assert!(run.success());
        assert!(!run.idle_expired);
        assert_eq!(run.stdout.bytes, b"ab");
    }

    #[test]
    fn shorter_timeout_wins_over_idle_output() {
        let scripts = Scripts::new();
        let script = scripts.write("idle-timeout.sh", "sleep 30\n");
        let started = Instant::now();
        let run = run(
            &mut command(&script),
            &idle_limits(Duration::from_millis(300), Some(Duration::from_secs(15))),
            &AtomicBool::new(false),
        );
        assert!(matches!(&run.end, End::TimedOut), "{:?}", run.end);
        assert!(!run.idle_expired);
        assert!(started.elapsed() < Duration::from_secs(10));
    }
}
