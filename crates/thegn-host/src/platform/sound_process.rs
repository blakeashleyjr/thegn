//! Owned, bounded lifecycle for notification sound helpers.
//!
//! Unix helpers run in a fresh process group and the group is killed before
//! the direct child is reaped. Windows deliberately keeps its prior direct
//! child behavior until atomic Job Object containment (THE-274) is available.

use std::time::{Duration, Instant};

/// Complete sound-helper deadline, including the process-group cleanup grace.
pub(crate) const SOUND_HELPER_DEADLINE: Duration = Duration::from_secs(5);
const POLL_INTERVAL: Duration = Duration::from_millis(10);
const CLEANUP_GRACE: Duration = Duration::from_millis(250);

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
                // Retain the leader as a waitable zombie until its group is
                // stopped, preventing its pid/pgid from being reused first.
                group.kill();
                let status = child.wait();
                let settled = settle_group(&group, deadline);
                return match (status, settled) {
                    (Ok(status), Ok(())) => SoundProcessOutcome::Exited(status),
                    (Err(error), _) => SoundProcessOutcome::Reap(error),
                    (_, Err(())) => SoundProcessOutcome::DescendantsRemain,
                };
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
            Duration::from_millis(500),
        );
        assert!(matches!(result, SoundProcessOutcome::Timeout));
    }

    #[test]
    fn early_parent_exit_does_not_leave_its_grandchild() {
        let dir = helper("sleep 30 & exit 0");
        let result = run_unix_bounded(
            dir.path().join("helper.sh").to_str().unwrap(),
            &[],
            &AtomicBool::new(false),
            Duration::from_secs(2),
        );
        assert!(matches!(result, SoundProcessOutcome::Exited(status) if status.success()));
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
            Duration::from_millis(500),
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
                let _ = child.kill();
                std::thread::spawn(move || {
                    #[expect(
                        clippy::disallowed_methods,
                        reason = "preserves monitor-action fallback on platforms without process-group ownership"
                    )]
                    let _ = child.wait(); // best-effort: teardown: a failed kill may leave the child running
                });
                return SoundProcessOutcome::Timeout;
            }
            Ok(None) => std::thread::sleep(POLL_INTERVAL),
            Err(error) => return SoundProcessOutcome::Reap(error),
        }
    }
}
