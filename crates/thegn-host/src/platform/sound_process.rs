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

/// Shared bounded subprocess implementation used by sound notification actions.
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
        let marker_dir = tempfile::tempdir().unwrap();
        let marker = marker_dir.path().join("ready");
        let dir = helper(&format!("sleep 30 & : > '{}'; exit 3", marker.display()));
        let helper_path = dir.path().join("helper.sh");
        let program = helper_path.to_str().unwrap().to_owned();
        let cancellation = std::sync::Arc::new(AtomicBool::new(false));
        let signal = std::sync::Arc::clone(&cancellation);
        let started = std::time::Instant::now();
        let canceller = std::thread::spawn(move || {
            let ready_deadline = std::time::Instant::now() + Duration::from_secs(30);
            while !marker.exists() {
                assert!(
                    std::time::Instant::now() < ready_deadline,
                    "helper never wrote its ready marker"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            std::thread::sleep(Duration::from_millis(500));
            signal.store(true, std::sync::atomic::Ordering::Release);
        });
        let result = run_bounded(&program, &[], &cancellation, Duration::from_secs(30));
        canceller.join().unwrap();
        assert!(
            matches!(result, SoundProcessOutcome::Exited(status) if status.code() == Some(3)),
            "{result:?} after {:?}",
            started.elapsed()
        );
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
            idle_expired: false,
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

    /// Move-only owner of both ends of a real PTY fixture. Moving this value
    /// transfers teardown responsibility as one unit. On macOS dropping it
    /// destroys the terminal before stopping and joining its master drainer.
    #[cfg(unix)]
    pub(crate) struct TestPtyFixture {
        terminal: Option<termwiz::terminal::UnixTerminal>,
        #[cfg(target_os = "macos")]
        drainer: Option<PtyDrainer>,
        #[cfg(not(target_os = "macos"))]
        master: Option<std::fs::File>,
    }

    #[cfg(unix)]
    impl Drop for TestPtyFixture {
        fn drop(&mut self) {
            // The terminal's drop can tcdrain, so its master reader stays live
            // until this first step completes. The drainer's thread owns the
            // master fd and drops it when the subsequent join returns.
            drop(self.terminal.take());
            #[cfg(target_os = "macos")]
            drop(self.drainer.take());
            #[cfg(not(target_os = "macos"))]
            drop(self.master.take());
        }
    }

    #[cfg(target_os = "macos")]
    struct PtyDrainer {
        stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
        idle: std::sync::Arc<std::sync::atomic::AtomicBool>,
        reader: Option<std::thread::JoinHandle<()>>,
    }

    #[cfg(target_os = "macos")]
    impl Drop for PtyDrainer {
        fn drop(&mut self) {
            use std::sync::atomic::Ordering;

            self.stop.store(true, Ordering::Release);
            if let Some(reader) = self.reader.take() {
                // The reader uses a finite poll timeout, so this join is
                // bounded even while the slave stays open and reads return
                // EAGAIN. It does not depend on EOF or EIO.
                let _ = reader.join();
            }
        }
    }

    #[cfg(target_os = "macos")]
    fn keep_master_drained(master: std::fs::File) -> PtyDrainer {
        use std::io::Read;
        use std::os::fd::AsRawFd;
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };

        let flags = unsafe { libc::fcntl(master.as_raw_fd(), libc::F_GETFL) };
        assert!(
            flags >= 0,
            "read PTY master flags: {}",
            std::io::Error::last_os_error()
        );
        assert_eq!(
            unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) },
            0,
            "make PTY master nonblocking: {}",
            std::io::Error::last_os_error()
        );

        let stop = Arc::new(AtomicBool::new(false));
        let reader_stop = stop.clone();
        let idle = Arc::new(AtomicBool::new(false));
        let reader_idle = idle.clone();
        let reader = std::thread::Builder::new()
            .name("thegn-test-pty-drain".into())
            .spawn(move || {
                let mut master = master;
                let mut bytes = [0_u8; 4096];
                loop {
                    if reader_stop.load(Ordering::Acquire) {
                        return;
                    }
                    loop {
                        match master.read(&mut bytes) {
                            Ok(0) => return,
                            Ok(_) => {
                                reader_idle.store(false, Ordering::Release);
                                continue;
                            }
                            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {
                                if reader_stop.load(Ordering::Acquire) {
                                    return;
                                }
                                continue;
                            }
                            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                                reader_idle.store(true, Ordering::Release);
                                break;
                            }
                            // A closed PTY can report EIO; no more output can
                            // be drained, and the fixture is being torn down.
                            Err(_) => return,
                        }
                    }

                    if reader_stop.load(Ordering::Acquire) {
                        return;
                    }
                    let mut pollfd = libc::pollfd {
                        fd: master.as_raw_fd(),
                        events: libc::POLLIN,
                        revents: 0,
                    };
                    // A finite timeout makes the stop flag observable without
                    // relying on terminal close or an indefinitely sleeping
                    // EAGAIN loop. Retry Interrupted without losing the stop.
                    loop {
                        // SAFETY: pollfd points to one initialized pollfd.
                        let result = unsafe { libc::poll(&mut pollfd, 1, 10) };
                        if result >= 0 {
                            break;
                        }
                        let error = std::io::Error::last_os_error();
                        if error.kind() == std::io::ErrorKind::Interrupted {
                            if reader_stop.load(Ordering::Acquire) {
                                return;
                            }
                            continue;
                        }
                        return;
                    }
                }
            })
            .expect("spawn PTY master drainer");
        PtyDrainer {
            stop,
            idle,
            reader: Some(reader),
        }
    }

    #[cfg(all(test, target_os = "macos"))]
    mod macos_tests {
        use super::*;
        use std::os::fd::FromRawFd;
        use std::sync::atomic::Ordering;
        use std::time::{Duration, Instant};

        #[test]
        fn pty_drainer_stops_while_slave_is_open_and_master_is_idle() {
            let mut master = -1;
            let mut slave = -1;
            assert_eq!(
                // SAFETY: out-pointers are valid; null optional attributes are allowed.
                unsafe {
                    libc::openpty(
                        &mut master,
                        &mut slave,
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                    )
                },
                0
            );
            // Keep the slave open so the master reader cannot finish via EOF/EIO.
            // SAFETY: openpty returned a fresh slave descriptor owned here.
            let _slave = unsafe { std::fs::File::from_raw_fd(slave) };
            // SAFETY: openpty returned a fresh master descriptor transferred
            // to the reader thread by keep_master_drained.
            let drainer = keep_master_drained(unsafe { std::fs::File::from_raw_fd(master) });

            let ready_by = Instant::now() + Duration::from_secs(1);
            while !drainer.idle.load(Ordering::Acquire) {
                assert!(Instant::now() < ready_by, "PTY reader did not reach EAGAIN");
                std::thread::yield_now();
            }

            let started = Instant::now();
            drop(drainer);
            assert!(
                started.elapsed() < Duration::from_secs(1),
                "PTY reader did not stop after its bounded poll interval"
            );
        }
    }

    /// A real terminal waker backed by a PTY. Keep the returned fixture alive
    /// as long as the waker is in use; it owns teardown ordering for both ends.
    #[cfg(unix)]
    pub(crate) fn test_waker() -> (termwiz::terminal::TerminalWaker, TestPtyFixture) {
        use std::os::fd::FromRawFd;
        use termwiz::terminal::UnixTerminal;
        let mut master = -1;
        let mut slave = -1;
        assert_eq!(
            // SAFETY: out-pointers are valid; null name/termios/winsize are allowed.
            unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
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
        #[cfg(target_os = "macos")]
        let fixture = TestPtyFixture {
            terminal: Some(terminal),
            drainer: Some(keep_master_drained(master)),
        };
        #[cfg(all(unix, not(target_os = "macos")))]
        let fixture = TestPtyFixture {
            terminal: Some(terminal),
            master: Some(master),
        };
        (fixture_terminal_waker(&fixture), fixture)
    }

    #[cfg(unix)]
    fn fixture_terminal_waker(fixture: &TestPtyFixture) -> termwiz::terminal::TerminalWaker {
        use termwiz::terminal::Terminal;
        fixture
            .terminal
            .as_ref()
            .expect("live PTY terminal")
            .waker()
    }

    #[cfg(not(unix))]
    pub(crate) fn test_waker() -> (termwiz::terminal::TerminalWaker, ()) {
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
