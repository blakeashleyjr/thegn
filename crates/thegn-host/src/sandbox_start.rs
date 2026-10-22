//! Starting a dormant container runtime — the subprocess half of
//! [`thegn_core::sandbox_dormant`], which decides *whether* to start one.
//!
//! Deliberately blocking with a hard deadline, and deliberately NOT loop-safe:
//! every caller is already off the event loop (`prepare_sandbox_env` runs on
//! `spawn_blocking`). `podman machine start` and `colima start` boot a VM, so
//! the budget is generous — but bounded, because a wedged hypervisor must not
//! hang a pane launch forever. A timed-out child is never killed; a background
//! thread waits for it so it is reaped when it exits.

use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// How long to wait for a runtime to come up. Booting a Linux VM on a cold
/// machine is tens of seconds; past this the user is better served by the modal
/// telling them to look at it themselves.
const START_TIMEOUT: Duration = Duration::from_secs(90);
const POLL: Duration = Duration::from_millis(200);
const MAX_PENDING_STARTS: usize = 2;

static PENDING: AtomicUsize = AtomicUsize::new(0);

enum Start {
    Exited(bool),
    SpawnFailed,
    WaitFailed,
    Refused,
    TimedOut(
        #[cfg_attr(
            not(test),
            expect(dead_code, reason = "the public caller detaches the reaper handle")
        )]
        Option<JoinHandle<std::io::Result<ExitStatus>>>,
    ),
}

/// Run a start command, returning whether it succeeded. Output is swallowed
/// (the progress line and the modal carry the story); failure is logged with the
/// command so `THEGN_LOG=info` explains a refusal. On timeout the child is left
/// running and reaped by a background thread.
///
/// Subprocess seam (`cov_ignore`); the argv it runs is chosen — and unit-tested —
/// in [`thegn_core::sandbox_dormant::start_argv`].
pub(crate) fn run(argv: &[String]) -> bool {
    matches!(
        run_with(argv, START_TIMEOUT, POLL, &PENDING),
        Start::Exited(true)
    )
}

fn admit(pending: usize) -> bool {
    pending < MAX_PENDING_STARTS
}

fn run_with(
    argv: &[String],
    timeout: Duration,
    poll: Duration,
    pending: &'static AtomicUsize,
) -> Start {
    if !admit(pending.load(Ordering::SeqCst)) {
        tracing::warn!(target: "thegn::sandbox", "a previous runtime start is still running; not starting another");
        return Start::Refused;
    }
    let Some((exe, args)) = argv.split_first() else {
        return Start::SpawnFailed;
    };
    let started = Instant::now();
    let child = Command::new(exe)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(target: "thegn::sandbox", cmd = %argv.join(" "), error = %e, "runtime start failed to spawn");
            return Start::SpawnFailed;
        }
    };
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let ok = status.success();
                if !ok {
                    tracing::warn!(target: "thegn::sandbox", cmd = %argv.join(" "), %status, "runtime start exited nonzero");
                }
                return Start::Exited(ok);
            }
            Ok(None) => {
                if started.elapsed() > timeout {
                    // Leave it running — a half-booted VM that finishes on its
                    // own is fine, and killing it would be worse than waiting.
                    tracing::warn!(target: "thegn::sandbox", cmd = %argv.join(" "), "runtime start timed out; leaving it to finish");
                    pending.fetch_add(1, Ordering::SeqCst);
                    let reaper = std::thread::Builder::new()
                        .name("thegn-sbx-reap".to_owned())
                        .spawn(move || {
                            crate::platform::qos::set_self(crate::platform::qos::Qos::Background);
                            #[expect(clippy::disallowed_methods, reason = "the timed-out child must be reaped without killing it")]
                            let result = child.wait();
                            match &result {
                                Ok(status) => tracing::info!(target: "thegn::sandbox", %status, "timed-out runtime start finished"),
                                Err(error) => tracing::warn!(target: "thegn::sandbox", %error, "timed-out runtime start could not be reaped"),
                            }
                            pending.fetch_sub(1, Ordering::SeqCst);
                            result
                        });
                    return match reaper {
                        Ok(handle) => Start::TimedOut(Some(handle)),
                        Err(error) => {
                            pending.fetch_sub(1, Ordering::SeqCst);
                            tracing::warn!(target: "thegn::sandbox", error = %error, "timed-out runtime start child cannot be reaped");
                            Start::TimedOut(None)
                        }
                    };
                }
                std::thread::sleep(poll);
            }
            Err(e) => {
                tracing::warn!(target: "thegn::sandbox", cmd = %argv.join(" "), error = %e, "runtime start wait failed");
                return Start::WaitFailed;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{MAX_PENDING_STARTS, Start, admit, run_with};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::thread;
    use std::time::{Duration, Instant};

    fn counter() -> &'static AtomicUsize {
        Box::leak(Box::new(AtomicUsize::new(0)))
    }

    #[test]
    fn timed_out_start_is_reaped_without_being_killed() {
        if !thegn_core::util::have("sleep") {
            return;
        }
        let pending = counter();
        let argv = ["sleep".to_owned(), "0.4".to_owned()];
        let result = run_with(
            &argv,
            Duration::from_millis(50),
            Duration::from_millis(10),
            pending,
        );
        let Start::TimedOut(Some(handle)) = result else {
            panic!("expected a timed-out start with a reaper");
        };
        assert_eq!(pending.load(Ordering::SeqCst), 1);
        let deadline = Instant::now() + Duration::from_secs(10);
        while !handle.is_finished() {
            assert!(
                Instant::now() < deadline,
                "reaper did not finish within 10 seconds"
            );
            thread::sleep(Duration::from_millis(10));
        }
        let result = handle.join().expect("reaper thread panicked");
        let status = result.expect("timed-out child wait failed");
        assert!(
            status.success(),
            "sleep was killed instead of being left to finish"
        );
        assert_eq!(pending.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn start_at_the_cap_is_refused_without_spawning() {
        if !thegn_core::util::have("sleep") {
            return;
        }
        let pending = counter();
        pending.store(MAX_PENDING_STARTS, Ordering::SeqCst);
        let argv = ["sleep".to_owned(), "5".to_owned()];
        let started = Instant::now();
        let result = run_with(
            &argv,
            Duration::from_secs(1),
            Duration::from_millis(10),
            pending,
        );
        assert!(matches!(result, Start::Refused));
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(pending.load(Ordering::SeqCst), MAX_PENDING_STARTS);
    }

    #[test]
    fn admit_table() {
        assert!(admit(0));
        assert!(admit(1));
        assert!(!admit(2));
        assert!(!admit(usize::MAX));
    }

    #[test]
    fn fast_exits_report_status_and_need_no_reaper() {
        let pending = counter();
        if thegn_core::util::have("true") {
            let result = run_with(
                &["true".to_owned()],
                Duration::from_secs(1),
                Duration::from_millis(10),
                pending,
            );
            assert!(matches!(result, Start::Exited(true)));
            let result = run_with(
                &["false".to_owned()],
                Duration::from_secs(1),
                Duration::from_millis(10),
                pending,
            );
            assert!(matches!(result, Start::Exited(false)));
        }
        let result = run_with(
            &[],
            Duration::from_secs(1),
            Duration::from_millis(10),
            pending,
        );
        assert!(matches!(result, Start::SpawnFailed));
        let result = run_with(
            &["thegn-missing-sandbox-start-executable".to_owned()],
            Duration::from_secs(1),
            Duration::from_millis(10),
            pending,
        );
        assert!(matches!(result, Start::SpawnFailed));
        assert_eq!(pending.load(Ordering::SeqCst), 0);
    }
}
