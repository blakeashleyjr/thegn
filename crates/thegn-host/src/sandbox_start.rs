//! Starting a dormant container runtime — the subprocess half of
//! [`thegn_core::sandbox_dormant`], which decides *whether* to start one.
//!
//! Deliberately blocking with a hard deadline, and deliberately NOT loop-safe:
//! every caller is already off the event loop (`prepare_sandbox_env` runs on
//! `spawn_blocking`). `podman machine start` and `colima start` boot a VM, so
//! the budget is generous — but bounded, because a wedged hypervisor must not
//! hang a pane launch forever.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// How long to wait for a runtime to come up. Booting a Linux VM on a cold
/// machine is tens of seconds; past this the user is better served by the modal
/// telling them to look at it themselves.
const START_TIMEOUT: Duration = Duration::from_secs(90);

/// Run a start command, returning whether it succeeded. Output is swallowed
/// (the progress line and the modal carry the story); failure is logged with the
/// command so `THEGN_LOG=info` explains a refusal.
///
/// Subprocess seam (`cov_ignore`); the argv it runs is chosen — and unit-tested —
/// in [`thegn_core::sandbox_dormant::start_argv`].
pub(crate) fn run(argv: &[String]) -> bool {
    let Some((exe, args)) = argv.split_first() else {
        return false;
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
            return false;
        }
    };
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let ok = status.success();
                if !ok {
                    tracing::warn!(target: "thegn::sandbox", cmd = %argv.join(" "), %status, "runtime start exited nonzero");
                }
                return ok;
            }
            Ok(None) => {
                if started.elapsed() > START_TIMEOUT {
                    // Leave it running — a half-booted VM that finishes on its
                    // own is fine, and killing it would be worse than waiting.
                    tracing::warn!(target: "thegn::sandbox", cmd = %argv.join(" "), "runtime start timed out; leaving it to finish");
                    return false;
                }
                std::thread::sleep(Duration::from_millis(200));
            }
            Err(e) => {
                tracing::warn!(target: "thegn::sandbox", cmd = %argv.join(" "), error = %e, "runtime start wait failed");
                return false;
            }
        }
    }
}
