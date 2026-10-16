use crate::{
    BACKOFF_CAP, Capture, Containment, End, GroupState, Leader, Limits, POLL_INTERVAL, Run, Stream,
    Which, aggregate_fit,
};
use nix::{
    errno::Errno,
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

struct Guard {
    pid: libc::pid_t,
    active: bool,
}

impl Drop for Guard {
    fn drop(&mut self) {
        if !self.active {
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

struct Pipes {
    stdout: Option<File>,
    stderr: Option<File>,
    out_open: bool,
    err_open: bool,
}

struct Accumulator {
    capture: Capture,
    stream: Stream,
}

impl Accumulator {
    fn read(&mut self, fd: libc::c_int) -> io::Result<(bool, bool)> {
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
            match self.stream {
                Stream::Null => {}
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

fn group_live(pid: libc::pid_t) -> bool {
    #[cfg(target_os = "linux")]
    {
        let Ok(entries) = std::fs::read_dir("/proc") else {
            return group_exists(pid);
        };
        for entry in entries.flatten() {
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if let Ok(member) = name.parse::<i32>() {
                let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
                    continue;
                };
                if let Some((_, rest)) = stat.rsplit_once(") ") {
                    let fields: Vec<_> = rest.split_whitespace().collect();
                    if fields.len() > 2
                        && fields[0] != "Z"
                        && fields.get(2).and_then(|field| field.parse::<i32>().ok()) == Some(pid)
                        && member != pid
                    {
                        return true;
                    }
                    if member == pid && fields.first().is_some_and(|state| *state != "Z") {
                        return true;
                    }
                }
            }
        }
        false
    }
    #[cfg(not(target_os = "linux"))]
    {
        group_exists(pid)
    }
}

fn group_exists(pid: libc::pid_t) -> bool {
    match killpg(Pid::from_raw(pid), None) {
        Ok(()) => true,
        Err(Errno::ESRCH) => false,
        Err(_) => true,
    }
}

fn poll_pipes(pipes: &Pipes, timeout: Duration) -> io::Result<bool> {
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
                .stderr
                .as_ref()
                .filter(|_| pipes.err_open)
                .map_or(-1, AsRawFd::as_raw_fd),
            events: libc::POLLIN | libc::POLLHUP,
            revents: 0,
        },
    ];
    let millis = timeout.as_millis().min(i32::MAX as u128) as i32;
    // SAFETY: fds points to two initialized pollfd records for the duration of poll.
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
    pipes: &mut Pipes,
    out: &mut Accumulator,
    err: &mut Accumulator,
) -> io::Result<Option<Which>> {
    let mut result = None;
    if pipes.out_open
        && let Some(pipe) = &pipes.stdout
    {
        let (_, overflow) = match out.read(pipe.as_raw_fd()) {
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
        let (_, overflow) = match err.read(pipe.as_raw_fd()) {
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

fn nap(pipes: &mut Pipes, backoff: Duration) -> io::Result<()> {
    if pipes.out_open || pipes.err_open {
        match poll_pipes(pipes, backoff) {
            Ok(_) => {}
            Err(_error) => {
                pipes.out_open = false;
                pipes.err_open = false;
                std::thread::sleep(backoff);
            }
        }
    } else {
        std::thread::sleep(backoff);
    }
    Ok(())
}

fn wait_empty(
    pid: libc::pid_t,
    deadline: Instant,
    pipes: &mut Pipes,
    out: &mut Accumulator,
    err: &mut Accumulator,
    backoff: &mut Duration,
) -> io::Result<bool> {
    loop {
        if !group_live(pid) {
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

fn reap_after(
    child: &mut Child,
    pid: libc::pid_t,
    kill_settle: Duration,
    signalled: bool,
) -> Leader {
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
            Ok(true) => {
                return match child.wait() {
                    Ok(status) => Leader::Reaped(status),
                    Err(error) => Leader::ReapFailed(error),
                };
            }
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

pub(super) fn run(cmd: &mut Command, limits: &Limits, cancel: &AtomicBool) -> Run {
    cmd.stdin(Stdio::null());
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
            return Run {
                end: End::Spawn(error),
                group: GroupState::Unknown,
                leader: Leader::NotSpawned,
                leader_exited_first: false,
                stdout: Capture::default(),
                stderr: Capture::default(),
                containment: Containment::ProcessGroup,
            };
        }
    };
    let pid = child.id() as libc::pid_t;
    let mut guard = Guard { pid, active: true };
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
    let mut pipes = Pipes {
        out_open: stdout.is_some(),
        err_open: stderr.is_some(),
        stdout,
        stderr,
    };
    for pipe in [&pipes.stdout, &pipes.stderr].into_iter().flatten() {
        if let Err(_error) = set_nonblocking(pipe) {
            send_group_signal(pid, Signal::SIGKILL);
            if let Err(_kill_error) = child.kill() {
                // The group signal above is the primary cleanup; wait still reaps the leader.
            }
            let leader = match child.wait() {
                Ok(status) => Leader::Reaped(status),
                Err(wait_error) => Leader::ReapFailed(wait_error),
            };
            guard.disarm();
            return Run {
                end: End::TimedOut,
                group: GroupState::Terminated,
                leader,
                leader_exited_first: false,
                stdout: Capture::default(),
                stderr: Capture::default(),
                containment: Containment::ProcessGroup,
            };
        }
    }
    let start = Instant::now();
    let deadline = start + limits.timeout;
    let mut out = Accumulator {
        capture: Capture::default(),
        stream: limits.stdout,
    };
    let mut err = Accumulator {
        capture: Capture::default(),
        stream: limits.stderr,
    };
    let mut backoff = POLL_INTERVAL;
    let mut leader_first = false;
    let mut end = loop {
        if cancel.load(Ordering::Acquire) {
            break End::Cancelled;
        }
        match leader_exited(pid) {
            Ok(true) => {
                leader_first = true;
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
                    if !group_live(pid) {
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
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if let Err(_error) = nap(&mut pipes, backoff.min(remaining)) {}
        backoff = crate::next_backoff(backoff, remaining);
    };
    let mut group = GroupState::Unknown;
    let mut signalled = false;
    if !matches!(end, End::LeaderExited | End::Supervise(_)) {
        signalled = true;
        if limits.term_grace > Duration::ZERO {
            send_group_signal(pid, Signal::SIGTERM);
            match wait_empty(
                pid,
                Instant::now() + limits.term_grace,
                &mut pipes,
                &mut out,
                &mut err,
                &mut backoff,
            ) {
                Ok(true) => group = GroupState::Terminated,
                Ok(false) => {}
                Err(_error) => {}
            }
        }
        if group != GroupState::Terminated {
            send_group_signal(pid, Signal::SIGKILL);
            group = match wait_empty(
                pid,
                Instant::now() + limits.kill_settle,
                &mut pipes,
                &mut out,
                &mut err,
                &mut backoff,
            ) {
                Ok(true) => GroupState::Terminated,
                Ok(false) => GroupState::Survivors,
                Err(_error) => GroupState::Survivors,
            };
        }
    } else if matches!(&end, End::LeaderExited) {
        group = if group_live(pid) {
            GroupState::Terminated
        } else {
            GroupState::Drained
        };
        if group == GroupState::Terminated {
            signalled = true;
            if limits.term_grace > Duration::ZERO {
                send_group_signal(pid, Signal::SIGTERM);
                match wait_empty(
                    pid,
                    Instant::now() + limits.term_grace,
                    &mut pipes,
                    &mut out,
                    &mut err,
                    &mut backoff,
                ) {
                    Ok(_) => {}
                    Err(_error) => {}
                }
            }
            send_group_signal(pid, Signal::SIGKILL);
            match wait_empty(
                pid,
                Instant::now() + limits.kill_settle,
                &mut pipes,
                &mut out,
                &mut err,
                &mut backoff,
            ) {
                Ok(false) => group = GroupState::Survivors,
                Ok(true) => group = GroupState::Terminated,
                Err(_error) => group = GroupState::Survivors,
            }
        }
    }
    let leader = reap_after(
        &mut child,
        pid,
        limits.kill_settle,
        signalled || leader_first,
    );
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
    let mut run = Run {
        end,
        group,
        leader,
        leader_exited_first: leader_first,
        stdout: out.capture,
        stderr: err.capture,
        containment: Containment::ProcessGroup,
    };
    aggregate_fit(&mut run.stdout, &mut run.stderr, limits.aggregate_tail_cap);
    guard.disarm();
    run
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        os::unix::fs::PermissionsExt,
        path::{Path, PathBuf},
        sync::{Arc, atomic::AtomicBool},
        thread,
    };

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

    #[test]
    fn leader_stays_a_zombie_until_its_group_drains() {
        if !Path::new("/proc/self/stat").exists() {
            return;
        }
        let dir = Scripts::new();
        let pid_file = dir.path().join("leader.pid");
        let script = dir.write(
            "zombie.sh",
            &format!("echo $$ > '{}'; sleep 1 & exit 0\n", pid_file.display()),
        );
        let configured = Limits {
            quiesce_after_leader: Some(Duration::from_secs(2)),
            ..limits(Duration::from_secs(3))
        };
        let script_for_thread = script.clone();
        let runner = thread::spawn(move || {
            run(
                &mut command(&script_for_thread),
                &configured,
                &AtomicBool::new(false),
            )
        });
        let deadline = Instant::now() + Duration::from_secs(1);
        let pid = loop {
            if let Ok(value) = fs::read_to_string(&pid_file) {
                break value.trim().parse::<u32>().unwrap();
            }
            assert!(Instant::now() < deadline, "leader pid was not written");
            thread::sleep(Duration::from_millis(10));
        };
        let stat_path = PathBuf::from(format!("/proc/{pid}/stat"));
        let zombie_deadline = Instant::now() + Duration::from_secs(1);
        loop {
            let stat = fs::read_to_string(&stat_path).unwrap();
            if stat.rsplit_once(") ").unwrap().1.starts_with('Z') {
                break;
            }
            assert!(
                Instant::now() < zombie_deadline,
                "leader was not pinned as a zombie"
            );
            thread::sleep(Duration::from_millis(10));
        }
        let result = runner.join().unwrap();
        assert_eq!(result.group, GroupState::Drained);
        assert!(!stat_path.exists());
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
        let script = dir.write("cancel-quiesce.sh", "sleep 30 & exit 0\n");
        let cancel = Arc::new(AtomicBool::new(false));
        let child_cancel = Arc::clone(&cancel);
        let runner = thread::spawn(move || {
            run(
                &mut command(&script),
                &limits(Duration::from_secs(10)),
                &child_cancel,
            )
        });
        thread::sleep(Duration::from_millis(150));
        cancel.store(true, Ordering::Release);
        let result = runner.join().unwrap();
        assert!(matches!(&result.end, End::Cancelled));
        assert!(result.leader_exited_first);
        assert_eq!(result.group, GroupState::Terminated);
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
        aggregate_fit(&mut stdout, &mut stderr, Some(8));
        assert_eq!(stdout.bytes.len(), 4);
        assert_eq!(stderr.bytes.len(), 4);
        assert!(stdout.truncated);
        assert!(!stderr.truncated);
    }

    #[test]
    fn limits_silent_defaults() {
        let configured = Limits::silent(Duration::from_secs(4));
        assert_eq!(configured.timeout, Duration::from_secs(4));
        assert_eq!(configured.quiesce_after_leader, None);
        assert_eq!(configured.term_grace, Duration::ZERO);
        assert_eq!(configured.kill_settle, Duration::from_millis(250));
        assert_eq!(configured.reader_grace, Duration::ZERO);
        assert_eq!(configured.stdout, Stream::Null);
        assert_eq!(configured.stderr, Stream::Null);
        assert_eq!(configured.aggregate_tail_cap, None);
    }
}
