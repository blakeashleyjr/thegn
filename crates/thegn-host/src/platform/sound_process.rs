//! Owned, bounded lifecycle for notification sound helpers.
//!
//! Unix helpers run in a fresh process group and the group is killed before
//! the direct child is reaped. Windows deliberately keeps its prior direct
//! child behavior until atomic Job Object containment (THE-274) is available.

use std::time::{Duration, Instant};

/// Sound-helper execution deadline plus the process-group cleanup grace. After
/// the execution part the whole process group is killed, so a command that
/// backgrounds work (`afplay x.wav &`) may keep playing until then.
pub(crate) const SOUND_HELPER_DEADLINE: Duration = Duration::from_secs(5);
const POLL_INTERVAL: Duration = Duration::from_millis(10);
const CLEANUP_GRACE: Duration = Duration::from_millis(250);
/// How long runtime shutdown waits for a worker to finish before detaching it.
/// A cancelled helper needs at most `CLEANUP_GRACE` to be killed and settled;
/// the margin covers scheduling. A worker stuck in uninterruptible I/O (a child
/// in D state, a hung pack directory, an unbounded Windows `status()`) cannot
/// be bounded from outside, so shutdown logs and detaches it past this point.
pub(crate) const SHUTDOWN_JOIN_BOUND: Duration = Duration::from_millis(500);

#[derive(Debug)]
pub(crate) enum SoundProcessOutcome {
    Exited(std::process::ExitStatus),
    Timeout,
    Cancelled,
    Spawn(std::io::Error),
    Reap(std::io::Error),
    DescendantsRemain,
}

#[cfg(unix)]
pub(crate) fn run(
    program: &str,
    args: &[String],
    cancellation: &std::sync::atomic::AtomicBool,
) -> SoundProcessOutcome {
    run_bounded(program, args, cancellation, SOUND_HELPER_DEADLINE)
}

/// Shared bounded subprocess implementation. The monitor-action caller keeps
/// its existing `Option<bool>` projection while sound maps every outcome.
#[cfg(unix)]
pub(crate) fn run_bounded(
    program: &str,
    args: &[String],
    cancellation: &std::sync::atomic::AtomicBool,
    timeout: Duration,
) -> SoundProcessOutcome {
    run_unix_bounded(program, args, cancellation, timeout)
}

#[cfg(unix)]
#[expect(
    clippy::disallowed_methods,
    reason = "every wait follows a confirmed exit or a group kill, on the dedicated sound worker thread"
)]
fn run_unix_bounded(
    program: &str,
    args: &[String],
    cancellation: &std::sync::atomic::AtomicBool,
    timeout: Duration,
) -> SoundProcessOutcome {
    use std::sync::atomic::Ordering;
    let deadline = Instant::now() + timeout;
    let execution_deadline = deadline.checked_sub(CLEANUP_GRACE).unwrap_or(deadline);
    let mut command = std::process::Command::new(program);
    command
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let (mut child, group) = match crate::platform::spawn_grouped(&mut command) {
        Ok(pair) => pair,
        Err(error) => return SoundProcessOutcome::Spawn(error),
    };

    loop {
        let cancelled = cancellation.load(Ordering::Acquire);
        let timed_out = Instant::now() >= execution_deadline;
        match crate::platform::gate_child_exited(&mut child) {
            Ok(true) => {
                // The leader has exited but is deliberately NOT reaped: the
                // unreaped zombie pins the pid/pgid so the group kill below can
                // never hit a recycled, unrelated group. It is reaped last.
                return drain_group(
                    &mut child,
                    &group,
                    cancellation,
                    execution_deadline,
                    deadline,
                );
            }
            Ok(false) if cancelled || timed_out => {
                group.kill();
                let status = child.wait();
                let cleanup_deadline = (Instant::now() + CLEANUP_GRACE).min(deadline);
                let settled = settle_group(&group, cleanup_deadline);
                return match (status, settled) {
                    (Err(error), _) => SoundProcessOutcome::Reap(error),
                    (_, Err(())) => SoundProcessOutcome::DescendantsRemain,
                    (Ok(_), Ok(())) if cancelled => SoundProcessOutcome::Cancelled,
                    (Ok(_), Ok(())) => SoundProcessOutcome::Timeout,
                };
            }
            Ok(false) => std::thread::sleep(POLL_INTERVAL),
            Err(error) => {
                group.kill();
                if let Err(reap_error) = child.wait() {
                    return SoundProcessOutcome::Reap(reap_error);
                }
                let cleanup_deadline = (Instant::now() + CLEANUP_GRACE).min(deadline);
                let settled = settle_group(&group, cleanup_deadline);
                return if settled.is_err() {
                    SoundProcessOutcome::DescendantsRemain
                } else {
                    SoundProcessOutcome::Reap(error)
                };
            }
        }
    }
}

/// Whether any non-zombie process other than the (zombie) leader remains in
/// the group. A zombie leader still answers `killpg(.., 0)`, so liveness needs
/// a process table scan; where none is available we conservatively report
/// "members remain", which only means the drain runs to the deadline.
#[cfg(unix)]
fn live_members(_group: &crate::platform::GroupHandle, _pgid: i32) -> bool {
    #[cfg(target_os = "linux")]
    {
        let pgid = _pgid;
        let Ok(dir) = std::fs::read_dir("/proc") else {
            return true;
        };
        for entry in dir.flatten() {
            let name = entry.file_name();
            if !name.to_string_lossy().bytes().all(|b| b.is_ascii_digit()) {
                continue;
            }
            // A process vanishing mid-scan is simply not a member any more.
            let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
                continue;
            };
            let Some(rest) = stat.rsplit_once(')').map(|(_, rest)| rest) else {
                continue;
            };
            let mut fields = rest.split_whitespace();
            let state = fields.next();
            let _ppid = fields.next();
            let pgrp = fields.next().and_then(|f| f.parse::<i32>().ok());
            if pgrp == Some(pgid) && state != Some("Z") {
                return true;
            }
        }
        false
    }
    #[cfg(not(target_os = "linux"))]
    {
        !_group.is_empty()
    }
}

/// After the leader exited (still unreaped): wait for the rest of its group to
/// drain on its own, killing it at the execution deadline or on cancellation
/// (cancellation shortens the post-kill settle to `CLEANUP_GRACE`). The leader
/// is reaped last and its status is the outcome.
#[cfg(unix)]
#[expect(
    clippy::disallowed_methods,
    reason = "the leader has already exited; this wait only reaps the zombie"
)]
fn drain_group(
    child: &mut std::process::Child,
    group: &crate::platform::GroupHandle,
    cancellation: &std::sync::atomic::AtomicBool,
    execution_deadline: Instant,
    deadline: Instant,
) -> SoundProcessOutcome {
    use std::sync::atomic::Ordering;
    let pgid = child.id() as i32;
    let mut settled = Ok(());
    loop {
        if !live_members(group, pgid) {
            break;
        }
        let cancelled = cancellation.load(Ordering::Acquire);
        if cancelled || Instant::now() >= execution_deadline {
            group.kill();
            let settle_deadline = if cancelled {
                (Instant::now() + CLEANUP_GRACE).min(deadline)
            } else {
                deadline
            };
            while live_members(group, pgid) {
                if Instant::now() >= settle_deadline {
                    settled = Err(());
                    break;
                }
                std::thread::sleep(POLL_INTERVAL);
            }
            break;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
    match (child.wait(), settled) {
        (Err(error), _) => SoundProcessOutcome::Reap(error),
        (Ok(_), Err(())) => SoundProcessOutcome::DescendantsRemain,
        (Ok(status), Ok(())) => SoundProcessOutcome::Exited(status),
    }
}

#[cfg(unix)]
fn settle_group(group: &crate::platform::GroupHandle, deadline: Instant) -> Result<(), ()> {
    while !group.is_empty() {
        if Instant::now() >= deadline {
            return Err(());
        }
        std::thread::sleep(POLL_INTERVAL);
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::AtomicBool;

    fn helper(contents: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("helper.sh");
        std::fs::write(&path, format!("#!/bin/sh\n{contents}\n")).unwrap();
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&path, permissions).unwrap();
        dir
    }

    #[test]
    fn deadline_kills_group_and_reaps_the_direct_child() {
        let dir = helper("sleep 30 & wait");
        let result = run_unix_bounded(
            dir.path().join("helper.sh").to_str().unwrap(),
            &[],
            &AtomicBool::new(false),
            Duration::from_secs(2),
        );
        assert!(matches!(result, SoundProcessOutcome::Timeout));
    }

    #[test]
    fn early_parent_exit_kills_its_grandchild_at_the_deadline() {
        let dir = helper("sleep 30 & exit 0");
        let started = Instant::now();
        let result = run_unix_bounded(
            dir.path().join("helper.sh").to_str().unwrap(),
            &[],
            &AtomicBool::new(false),
            Duration::from_millis(600),
        );
        assert!(matches!(result, SoundProcessOutcome::Exited(status) if status.success()));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn leader_stays_an_unreaped_zombie_until_its_group_is_drained() {
        let dir = helper("echo $$ > \"$1\"; sleep 1 & exit 0");
        let pid_file = dir.path().join("pid");
        let script = dir.path().join("helper.sh").to_str().unwrap().to_string();
        let args = vec![pid_file.display().to_string()];
        let runner = std::thread::spawn(move || {
            run_unix_bounded(
                &script,
                &args,
                &AtomicBool::new(false),
                Duration::from_secs(5),
            )
        });
        let started = Instant::now();
        let pid = loop {
            if let Some(pid) = std::fs::read_to_string(&pid_file)
                .ok()
                .and_then(|s| s.trim().parse::<i32>().ok())
            {
                break pid;
            }
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "helper never started"
            );
            std::thread::sleep(Duration::from_millis(5));
        };
        // The leader exits immediately; the backgrounded `sleep` keeps the group alive.
        std::thread::sleep(Duration::from_millis(400));
        let state = test_support::proc_state(pid);
        if test_support::HAS_PROC {
            assert_eq!(
                state,
                Some('Z'),
                "leader must stay an unreaped zombie while its group drains"
            );
        }
        let result = runner.join().unwrap();
        assert!(matches!(result, SoundProcessOutcome::Exited(status) if status.success()));
        assert!(
            test_support::pid_gone(pid),
            "leader must be reaped at the end"
        );
    }

    #[test]
    fn leader_exit_lets_the_group_drain_before_the_deadline() {
        let dir = helper("(sleep 0.3; touch \"$1\") & exit 0");
        let marker = dir.path().join("played");
        let result = run_unix_bounded(
            dir.path().join("helper.sh").to_str().unwrap(),
            &[marker.display().to_string()],
            &AtomicBool::new(false),
            Duration::from_secs(5),
        );
        assert!(matches!(result, SoundProcessOutcome::Exited(status) if status.success()));
        assert!(
            marker.exists(),
            "backgrounded work must be allowed to finish"
        );
    }

    #[test]
    fn cancellation_kills_and_reaps_active_group() {
        let dir = helper("sleep 30 & wait");
        let cancellation = AtomicBool::new(true);
        let result = run_unix_bounded(
            dir.path().join("helper.sh").to_str().unwrap(),
            &[],
            &cancellation,
            Duration::from_secs(2),
        );
        assert!(matches!(result, SoundProcessOutcome::Cancelled));
    }

    #[test]
    fn timed_out_helper_does_not_prevent_the_next_helper() {
        let hanging = helper("sleep 30 & wait");
        let succeeds = helper("exit 0");
        let first = run_unix_bounded(
            hanging.path().join("helper.sh").to_str().unwrap(),
            &[],
            &AtomicBool::new(false),
            Duration::from_secs(2),
        );
        assert!(matches!(first, SoundProcessOutcome::Timeout));
        let second = run_unix_bounded(
            succeeds.path().join("helper.sh").to_str().unwrap(),
            &[],
            &AtomicBool::new(false),
            Duration::from_secs(2),
        );
        assert!(matches!(second, SoundProcessOutcome::Exited(status) if status.success()));
    }
}

#[cfg(not(unix))]
#[expect(
    clippy::disallowed_methods,
    reason = "Windows sound retains its pre-existing direct-child wait until THE-274"
)]
pub(crate) fn run(
    program: &str,
    args: &[String],
    _cancellation: &std::sync::atomic::AtomicBool,
) -> SoundProcessOutcome {
    // THE-274 owns atomic Windows job assignment and fail-closed containment.
    // Preserve the previous direct-child status behavior until that seam lands.
    let mut command = std::process::Command::new(program);
    command
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    match command.status() {
        Ok(status) => SoundProcessOutcome::Exited(status),
        Err(error) => SoundProcessOutcome::Spawn(error),
    }
}

/// The existing monitor-action fallback on platforms without the Unix process
/// group seam. Sound itself continues to use its prior direct-child behavior.
#[cfg(not(unix))]
pub(crate) fn run_bounded(
    program: &str,
    args: &[String],
    _cancellation: &std::sync::atomic::AtomicBool,
    timeout: Duration,
) -> SoundProcessOutcome {
    let mut child = match std::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => return SoundProcessOutcome::Spawn(error),
    };
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return SoundProcessOutcome::Exited(status),
            Ok(None) if Instant::now() >= deadline => {
                if let Err(error) = child.kill() {
                    tracing::debug!(target: "thegn::notify_sound", %error, "sound helper kill failed");
                }
                std::thread::spawn(move || {
                    #[expect(
                        clippy::disallowed_methods,
                        reason = "preserves monitor-action fallback on platforms without process-group ownership"
                    )]
                    if let Err(error) = child.wait() {
                        tracing::debug!(target: "thegn::notify_sound", %error, "sound helper reap failed");
                    }
                });
                return SoundProcessOutcome::Timeout;
            }
            Ok(None) => std::thread::sleep(POLL_INTERVAL),
            Err(error) => return SoundProcessOutcome::Reap(error),
        }
    }
}

/// Test-only helpers so callers' tests stay free of platform `#[cfg]`.
#[cfg(test)]
pub(crate) mod test_support {
    pub(crate) const UNIX: bool = cfg!(unix);
    pub(crate) const HAS_PROC: bool = cfg!(target_os = "linux");

    /// A real terminal waker (backed by a pty); the other ends must outlive it.
    #[cfg(unix)]
    pub(crate) fn test_waker() -> (
        termwiz::terminal::TerminalWaker,
        std::fs::File,
        termwiz::terminal::UnixTerminal,
    ) {
        use std::os::fd::FromRawFd;
        use termwiz::terminal::{Terminal, UnixTerminal};
        let mut master = -1;
        let mut slave = -1;
        assert_eq!(
            // SAFETY: out-pointers are valid; null name/termios/winsize are allowed.
            unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    std::ptr::null(),
                )
            },
            0
        );
        // SAFETY: openpty returned two fresh fds that we now own.
        let master = unsafe { std::fs::File::from_raw_fd(master) };
        let slave = unsafe { std::fs::File::from_raw_fd(slave) };
        let caps =
            termwiz::caps::Capabilities::new_with_hints(termwiz::caps::ProbeHints::default())
                .unwrap();
        let terminal = UnixTerminal::new_with(caps, &slave, &slave).unwrap();
        (terminal.waker(), master, terminal)
    }

    #[cfg(not(unix))]
    pub(crate) fn test_waker() -> (termwiz::terminal::TerminalWaker, (), ()) {
        unreachable!("callers return early when !UNIX")
    }

    /// The `/proc` state letter of `pid` (Linux only; `None` elsewhere or if gone).
    pub(crate) fn proc_state(_pid: i32) -> Option<char> {
        #[cfg(target_os = "linux")]
        {
            let stat = std::fs::read_to_string(format!("/proc/{_pid}/stat")).ok()?;
            stat.rsplit_once(')')?.1.trim_start().chars().next()
        }
        #[cfg(not(target_os = "linux"))]
        {
            None
        }
    }

    /// True when `pid` no longer exists at all (a zombie still answers signal 0).
    #[cfg(unix)]
    pub(crate) fn pid_gone(pid: i32) -> bool {
        // SAFETY: signal 0 only probes for existence.
        let rc = unsafe { libc::kill(pid, 0) };
        let errno = std::io::Error::last_os_error().raw_os_error();
        rc == -1 && errno == Some(libc::ESRCH)
    }

    #[cfg(not(unix))]
    pub(crate) fn pid_gone(_pid: i32) -> bool {
        true
    }
}
