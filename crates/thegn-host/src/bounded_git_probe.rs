//! Shared bounded capture for fixed local Git and capability probes, not gates.
//! Git retains its two-slot budget; capability probes have a separate one-slot
//! budget. Each capture runs on `thegn_proc` (own process group, pipes read on
//! the calling thread, leader reaped before return); a slot is consumed for the
//! process lifetime only when a child or group member could not be confirmed gone.
//! OS spawn/filesystem calls are not interruptible; this is not a general command sandbox or cancellation lease.

use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use thegn_proc::{End, GroupState, InputFeed, Leader, Limits, Run, Stream};

static GIT_IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);
static CAPABILITY_IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);

#[derive(Clone, Copy)]
enum CaptureLane {
    Git,
    Capability,
}
impl CaptureLane {
    fn prefix(self) -> &'static str {
        match self {
            Self::Git => "Git",
            Self::Capability => "capability probe",
        }
    }
}

#[derive(Debug)]
pub(crate) struct ProbeError(String);
impl std::fmt::Display for ProbeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for ProbeError {}
fn probe_error(message: impl Into<String>) -> ProbeError {
    ProbeError(message.into())
}

pub(crate) fn capture(
    command: std::process::Command,
    input: Option<String>,
    label: &str,
) -> Result<Vec<u8>, ProbeError> {
    capture_with(
        command,
        input,
        label,
        std::time::Duration::from_secs(15),
        &GIT_IN_FLIGHT,
    )
}

/// Fixed local capability queries run off the compositor. Unlike Git probes,
/// a nonzero process exit is data for the provider's existing status policy.
/// Output, child lifetime and retained ownership are bounded independently of
/// Git, so a stuck provider cannot occupy the Git lane or its two slots.
pub(crate) fn capture_capability(
    command: std::process::Command,
    timeout: std::time::Duration,
    byte_limit: usize,
) -> Result<std::process::Output, ProbeError> {
    if timeout > std::time::Duration::from_secs(5) || byte_limit > 16 * 1024 {
        return Err(probe_error(
            "capability capture exceeds its fixed safety policy",
        ));
    }
    capture_output_with(
        command,
        None,
        timeout,
        byte_limit,
        &CAPABILITY_IN_FLIGHT,
        CaptureLane::Capability,
    )
}

#[derive(Debug)]
struct GitBudget(&'static AtomicUsize);
impl Drop for GitBudget {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}
fn git_budget(counter: &'static AtomicUsize) -> Result<Arc<GitBudget>, ProbeError> {
    budget_for(counter, CaptureLane::Git)
}
fn budget_for(
    counter: &'static AtomicUsize,
    lane: CaptureLane,
) -> Result<Arc<GitBudget>, ProbeError> {
    let slots = match lane {
        CaptureLane::Git => 2,
        CaptureLane::Capability => 1,
    };
    counter
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
            (n < slots).then_some(n + 1)
        })
        .map(|_| Arc::new(GitBudget(counter)))
        .map_err(|_| {
            probe_error(match lane {
                CaptureLane::Git => {
                    "cleanup Git capacity held by unfinished child/pipe; retry later"
                }
                CaptureLane::Capability => {
                    "capability capacity held by unfinished child/pipe; retry later"
                }
            })
        })
}

fn capture_with(
    command: std::process::Command,
    input: Option<String>,
    label: &str,
    timeout: std::time::Duration,
    counter: &'static AtomicUsize,
) -> Result<Vec<u8>, ProbeError> {
    let output = capture_output_with(
        command,
        input,
        timeout,
        2 * 1024 * 1024,
        counter,
        CaptureLane::Git,
    )?;
    if !output.status.success() {
        return Err(probe_error(format!(
            "Git {} refused: {}",
            label,
            String::from_utf8_lossy(&output.stderr)
                .chars()
                .take(512)
                .collect::<String>()
        )));
    }
    Ok(output.stdout)
}

fn capture_output_with(
    mut command: std::process::Command,
    input: Option<String>,
    timeout: std::time::Duration,
    byte_limit: usize,
    counter: &'static AtomicUsize,
    lane: CaptureLane,
) -> Result<std::process::Output, ProbeError> {
    use std::time::Duration;
    if timeout.is_zero() {
        return Err(probe_error("invalid capture deadline"));
    }
    if byte_limit == 0 {
        return Err(probe_error("invalid capture byte bound"));
    }
    let budget = match lane {
        CaptureLane::Git => git_budget(counter)?,
        CaptureLane::Capability => budget_for(counter, lane)?,
    };
    let limits = Limits {
        stdout: Stream::HeadOrFail { cap: byte_limit },
        stderr: Stream::HeadOrFail { cap: byte_limit },
        quiesce_after_leader: Some(Duration::ZERO),
        term_grace: Duration::from_millis(200),
        reader_grace: Duration::from_millis(250),
        ..Limits::silent(timeout)
    };
    let cancel = AtomicBool::new(false);
    let (run, feed) = match input {
        Some(input) => {
            let (run, feed) =
                thegn_proc::run_with_input(&mut command, &limits, input.as_bytes(), &cancel);
            (run, Some(feed))
        }
        None => (thegn_proc::run(&mut command, &limits, &cancel), None),
    };
    if releases_budget(&run) {
        drop(budget);
    } else {
        // Never release capacity for a child (or group member) that was not
        // observed reaped and gone: the slot stays consumed for the process.
        std::mem::forget(budget);
    }
    map_capture(run, feed, lane.prefix())
}

/// Capacity is returned only when nothing can outlive the capture: the leader
/// never ran, or it was reaped and its process group was observed empty.
fn releases_budget(run: &Run) -> bool {
    match (&run.leader, run.group) {
        (Leader::NotSpawned, _) => matches!(run.end, End::Spawn(_)),
        (Leader::Reaped(_), GroupState::Drained | GroupState::Terminated) => true,
        _ => false,
    }
}

/// Map one bounded run to the probe contract, in fixed precedence: spawn,
/// timeout, output bound, supervision, reap, stdin feed, then the output.
fn map_capture(
    run: Run,
    feed: Option<InputFeed>,
    prefix: &str,
) -> Result<std::process::Output, ProbeError> {
    match run.end {
        End::Spawn(error) => return Err(probe_error(format!("{prefix} could not start: {error}"))),
        End::TimedOut => {
            return Err(probe_error(format!(
                "{prefix} operation exceeded cleanup deadline"
            )));
        }
        End::OutputLimit(_) => {
            return Err(probe_error(format!(
                "{prefix} probe output exceeded the safety bound"
            )));
        }
        End::Cancelled => return Err(probe_error(format!("{prefix} wait failed: cancelled"))),
        End::Supervise(error) => return Err(probe_error(format!("{prefix} wait failed: {error}"))),
        End::LeaderExited => {}
    }
    let status = match run.leader {
        Leader::Reaped(status) => status,
        Leader::ReapFailed(error) => {
            return Err(probe_error(format!("{prefix} wait failed: {error}")));
        }
        Leader::Detached | Leader::NotSpawned => {
            return Err(probe_error(format!("{prefix} wait failed")));
        }
    };
    if let Some(InputFeed {
        complete: false,
        error: Some(kind),
        ..
    }) = feed
    {
        return Err(probe_error(format!(
            "{prefix} input failed: {}",
            std::io::Error::from(kind)
        )));
    }
    Ok(std::process::Output {
        status,
        stdout: run.stdout.bytes,
        stderr: run.stderr.bytes,
    })
}

#[cfg(test)]
#[path = "bounded_git_probe_tests.rs"]
mod tests;
