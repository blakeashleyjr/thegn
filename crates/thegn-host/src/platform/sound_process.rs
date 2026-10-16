//! Owned, bounded lifecycle for notification sound helpers.
//!
//! Helpers use the shared bounded process runner. Unix helpers run in a fresh
//! process group and the group is killed before the direct child is reaped.
//! Windows uses the crate's bounded direct-child fallback (deadline, kill, and
//! reap, with cancellation); atomic Job Object containment remains THE-274.

use std::time::Duration;

/// Sound-helper execution deadline plus the process-group cleanup grace. After
/// the execution part the whole process group is killed, so a command that
/// backgrounds work (`afplay x.wav &`) may keep playing until then.
pub(crate) const SOUND_HELPER_DEADLINE: Duration = Duration::from_secs(5);
const CLEANUP_GRACE: Duration = Duration::from_millis(250);
/// How long runtime shutdown waits for a worker to finish before detaching it.
/// A cancelled helper needs at most `CLEANUP_GRACE` to be killed and settled;
/// the margin covers scheduling. A worker stuck in uninterruptible I/O (a child
/// in D state or a hung pack directory) cannot be bounded from outside, so
/// shutdown logs and detaches it past this point.
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

pub(crate) fn run(
    program: &str,
    args: &[String],
    cancellation: &std::sync::atomic::AtomicBool,
) -> SoundProcessOutcome {
    run_bounded(program, args, cancellation, SOUND_HELPER_DEADLINE)
}

/// Shared bounded subprocess implementation. The monitor-action caller keeps
/// its existing `Option<bool>` projection while sound maps every outcome.
pub(crate) fn run_bounded(
    program: &str,
    args: &[String],
    cancellation: &std::sync::atomic::AtomicBool,
    timeout: Duration,
) -> SoundProcessOutcome {
    let mut command = std::process::Command::new(program);
    command.args(args);
    let limits = thegn_proc::Limits {
        kill_settle: CLEANUP_GRACE,
        ..thegn_proc::Limits::silent(timeout.checked_sub(CLEANUP_GRACE).unwrap_or(timeout))
    };
    outcome(thegn_proc::run(&mut command, &limits, cancellation))
}

fn outcome(run: thegn_proc::Run) -> SoundProcessOutcome {
    let thegn_proc::Run {
        end,
        group,
        leader,
        leader_exited_first,
        ..
    } = run;
    match (end, group, leader, leader_exited_first) {
        (thegn_proc::End::Spawn(error), _, _, _) => SoundProcessOutcome::Spawn(error),
        (_, _, thegn_proc::Leader::ReapFailed(error), _) => SoundProcessOutcome::Reap(error),
        (thegn_proc::End::Supervise(error), _, _, _) => SoundProcessOutcome::Reap(error),
        (_, thegn_proc::GroupState::Survivors, _, _) | (_, _, thegn_proc::Leader::Detached, _) => {
            SoundProcessOutcome::DescendantsRemain
        }
        (thegn_proc::End::LeaderExited, _, thegn_proc::Leader::Reaped(status), _) => {
            SoundProcessOutcome::Exited(status)
        }
        (thegn_proc::End::Cancelled, _, thegn_proc::Leader::Reaped(status), true) => {
            SoundProcessOutcome::Exited(status)
        }
        (thegn_proc::End::Cancelled, _, _, _) => SoundProcessOutcome::Cancelled,
        (thegn_proc::End::TimedOut, _, _, _) => SoundProcessOutcome::Timeout,
        (thegn_proc::End::OutputLimit(_), _, _, _) => {
            // Null streams make OutputLimit impossible; preserve a bounded failure mapping.
            SoundProcessOutcome::Timeout
        }
        (thegn_proc::End::LeaderExited, _, thegn_proc::Leader::NotSpawned, _) => {
            // The crate never produces LeaderExited when the leader was not spawned.
            SoundProcessOutcome::Reap(std::io::Error::other("leader not spawned"))
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::{fs::PermissionsExt, process::ExitStatusExt};
    use std::sync::atomic::AtomicBool;

    fn helper(contents: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let mut dir_permissions = std::fs::metadata(dir.path()).unwrap().permissions();
        dir_permissions.set_mode(0o700);
        std::fs::set_permissions(dir.path(), dir_permissions).unwrap();
        let path = dir.path().join("helper.sh");
        std::fs::write(&path, format!("#!/bin/sh\n{contents}\n")).unwrap();
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&path, permissions).unwrap();
        dir
    }

    #[test]
    fn early_parent_exit_kills_its_grandchild_at_the_deadline() {
        let dir = helper("sleep 30 & exit 0");
        let helper_path = dir.path().join("helper.sh");
        let program = helper_path.to_str().unwrap();
        let started = std::time::Instant::now();
        let result = run_bounded(
            program,
            &[],
            &AtomicBool::new(false),
            Duration::from_millis(600),
        );
        assert!(matches!(result, SoundProcessOutcome::Exited(status) if status.success()));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn hung_helper_maps_to_timeout() {
        let dir = helper("sleep 30 & wait");
        let helper_path = dir.path().join("helper.sh");
        let program = helper_path.to_str().unwrap();
        let result = run_bounded(
            program,
            &[],
            &AtomicBool::new(false),
            Duration::from_secs(1),
        );
        assert!(matches!(result, SoundProcessOutcome::Timeout));
    }

    #[test]
    fn pre_set_cancellation_maps_to_cancelled() {
        let dir = helper("sleep 30 & wait");
        let helper_path = dir.path().join("helper.sh");
        let program = helper_path.to_str().unwrap();
        let cancellation = AtomicBool::new(true);
        let result = run_bounded(program, &[], &cancellation, Duration::from_secs(2));
        assert!(matches!(result, SoundProcessOutcome::Cancelled));
    }

    #[test]
    fn cancel_after_leader_exit_maps_to_exited() {
        let dir = helper("sleep 30 & exit 3");
        let helper_path = dir.path().join("helper.sh");
        let program = helper_path.to_str().unwrap().to_owned();
        let cancellation = std::sync::Arc::new(AtomicBool::new(false));
        let signal = std::sync::Arc::clone(&cancellation);
        let canceller = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            signal.store(true, std::sync::atomic::Ordering::Release);
        });
        let result = run_bounded(&program, &[], &cancellation, Duration::from_secs(5));
        canceller.join().unwrap();
        assert!(matches!(result, SoundProcessOutcome::Exited(status) if status.code() == Some(3)));
    }

    #[test]
    fn missing_program_maps_to_spawn() {
        let dir = tempfile::tempdir().unwrap();
        let mut permissions = std::fs::metadata(dir.path()).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(dir.path(), permissions).unwrap();
        let missing = dir.path().join("missing-program");
        let program = missing.to_str().unwrap();
        let result = run_bounded(
            program,
            &[],
            &AtomicBool::new(false),
            Duration::from_secs(1),
        );
        assert!(
            matches!(result, SoundProcessOutcome::Spawn(error) if error.kind() == std::io::ErrorKind::NotFound)
        );
    }

    #[test]
    fn failing_helper_maps_to_exited_failure() {
        let dir = helper("exit 7");
        let helper_path = dir.path().join("helper.sh");
        let program = helper_path.to_str().unwrap();
        let result = run_bounded(
            program,
            &[],
            &AtomicBool::new(false),
            Duration::from_secs(2),
        );
        assert!(matches!(result, SoundProcessOutcome::Exited(status) if status.code() == Some(7)));
    }

    fn make_run(
        end: thegn_proc::End,
        group: thegn_proc::GroupState,
        leader: thegn_proc::Leader,
        leader_exited_first: bool,
    ) -> thegn_proc::Run {
        thegn_proc::Run {
            end,
            group,
            leader,
            leader_exited_first,
            stdout: thegn_proc::Capture::default(),
            stderr: thegn_proc::Capture::default(),
            containment: thegn_proc::Containment::ProcessGroup,
        }
    }

    fn success_status() -> std::process::ExitStatus {
        std::process::ExitStatus::from_raw(0)
    }

    #[test]
    fn outcome_survivors_map_to_descendants_remain() {
        let run = make_run(
            thegn_proc::End::TimedOut,
            thegn_proc::GroupState::Survivors,
            thegn_proc::Leader::Reaped(success_status()),
            false,
        );
        assert!(matches!(
            outcome(run),
            SoundProcessOutcome::DescendantsRemain
        ));
    }

    #[test]
    fn outcome_detached_maps_to_descendants_remain() {
        let run = make_run(
            thegn_proc::End::TimedOut,
            thegn_proc::GroupState::Terminated,
            thegn_proc::Leader::Detached,
            false,
        );
        assert!(matches!(
            outcome(run),
            SoundProcessOutcome::DescendantsRemain
        ));
    }

    #[test]
    fn outcome_reap_failure_precedes_timeout() {
        let run = make_run(
            thegn_proc::End::TimedOut,
            thegn_proc::GroupState::Drained,
            thegn_proc::Leader::ReapFailed(std::io::Error::other("reap")),
            false,
        );
        assert!(matches!(outcome(run), SoundProcessOutcome::Reap(_)));
    }

    #[test]
    fn outcome_supervise_maps_to_reap() {
        let run = make_run(
            thegn_proc::End::Supervise(std::io::Error::other("waitid")),
            thegn_proc::GroupState::Unknown,
            thegn_proc::Leader::Detached,
            false,
        );
        assert!(matches!(outcome(run), SoundProcessOutcome::Reap(_)));
    }

    #[test]
    fn outcome_spawn_maps_to_spawn() {
        let run = make_run(
            thegn_proc::End::Spawn(std::io::Error::other("spawn")),
            thegn_proc::GroupState::Unknown,
            thegn_proc::Leader::NotSpawned,
            false,
        );
        assert!(matches!(outcome(run), SoundProcessOutcome::Spawn(_)));
    }

    #[test]
    fn outcome_cancelled_after_leader_exit_maps_to_exited() {
        let run = make_run(
            thegn_proc::End::Cancelled,
            thegn_proc::GroupState::Terminated,
            thegn_proc::Leader::Reaped(success_status()),
            true,
        );
        assert!(matches!(outcome(run), SoundProcessOutcome::Exited(status) if status.success()));
    }

    #[test]
    fn outcome_cancelled_before_leader_exit_maps_to_cancelled() {
        let run = make_run(
            thegn_proc::End::Cancelled,
            thegn_proc::GroupState::Terminated,
            thegn_proc::Leader::Reaped(success_status()),
            false,
        );
        assert!(matches!(outcome(run), SoundProcessOutcome::Cancelled));
    }

    #[test]
    fn outcome_timed_out_with_reaped_leader_maps_to_timeout() {
        let run = make_run(
            thegn_proc::End::TimedOut,
            thegn_proc::GroupState::Drained,
            thegn_proc::Leader::Reaped(success_status()),
            false,
        );
        assert!(matches!(outcome(run), SoundProcessOutcome::Timeout));
    }

    #[test]
    fn outcome_leader_exit_after_termination_maps_to_exited() {
        let run = make_run(
            thegn_proc::End::LeaderExited,
            thegn_proc::GroupState::Terminated,
            thegn_proc::Leader::Reaped(success_status()),
            true,
        );
        assert!(matches!(outcome(run), SoundProcessOutcome::Exited(status) if status.success()));
    }

    #[test]
    fn outcome_leader_exit_without_spawned_leader_maps_to_reap() {
        let run = make_run(
            thegn_proc::End::LeaderExited,
            thegn_proc::GroupState::Unknown,
            thegn_proc::Leader::NotSpawned,
            false,
        );
        assert!(
            matches!(outcome(run), SoundProcessOutcome::Reap(error) if error.to_string() == "leader not spawned")
        );
    }
}

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
