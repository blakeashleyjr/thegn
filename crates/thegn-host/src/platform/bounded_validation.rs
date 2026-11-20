//! Bounded, fully reaped execution of one unattended validation command (the
//! pipeline stage executor's sandboxed `validate` step).
//!
//! The bounded group runner (`agent_run::bounded`) is unix-only: it owns a real
//! process group, kills it on timeout or cancel and reaps it. Off unix there is
//! no equivalent supervision, so validation refuses rather than run unbounded
//! on the host. The caller (`daemon/stage_executor.rs`) stays platform-free and
//! maps the portable `ValidationEnd` to its own result.

use std::process::Command;
use std::sync::atomic::AtomicBool;

/// Whether this platform can run an unattended validation at all. When
/// `false`, callers refuse before building a sandbox entry.
pub(crate) const SUPPORTED: bool = cfg!(unix);

/// How a bounded validation run ended, without platform types.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ValidationEnd {
    /// The leader exited; `None` when it died to a signal.
    Exited(Option<i32>),
    /// The configured deadline elapsed and the group was stopped.
    TimedOut,
    /// The run was cancelled (halt).
    Cancelled,
    /// The run could not be supervised (spawn, reap, a group that would not
    /// settle, or no bounded runner on this platform), already worded.
    Failed(String),
}

/// One finished validation run: its end and the retained stdout + stderr.
#[derive(Debug)]
pub(crate) struct ValidationRun {
    pub(crate) end: ValidationEnd,
    pub(crate) text: String,
}

/// Run `cmd` in its own process group to a bounded, fully reaped end.
/// `timeout_secs = 0` gets the agent ceiling, never "unbounded".
#[cfg(unix)]
pub(crate) fn run(cmd: &mut Command, timeout_secs: u64, cancel: &AtomicBool) -> ValidationRun {
    use crate::agent_run::bounded;
    let run = bounded::run_bounded(cmd, bounded::effective_timeout(timeout_secs), cancel);
    let mut text = String::from_utf8_lossy(&run.stdout.bytes).into_owned();
    text.push_str(&String::from_utf8_lossy(&run.stderr.bytes));
    ValidationRun {
        end: end_of(&run.outcome),
        text,
    }
}

/// No bounded group runner off unix: refuse, never run on the host.
#[cfg(not(unix))]
pub(crate) fn run(_cmd: &mut Command, _timeout_secs: u64, _cancel: &AtomicBool) -> ValidationRun {
    ValidationRun {
        end: ValidationEnd::Failed("unsupported on this platform".to_owned()),
        text: String::new(),
    }
}

#[cfg(unix)]
fn end_of(outcome: &crate::agent_run::bounded::AgentRunOutcome) -> ValidationEnd {
    use crate::agent_run::bounded::AgentRunOutcome;
    match outcome {
        AgentRunOutcome::Exited(status) => ValidationEnd::Exited(status.code()),
        AgentRunOutcome::TimedOut => ValidationEnd::TimedOut,
        AgentRunOutcome::Cancelled => ValidationEnd::Cancelled,
        AgentRunOutcome::Spawn(error) => {
            ValidationEnd::Failed(format!("the validation sandbox failed to spawn: {error}"))
        }
        AgentRunOutcome::Reap(error) => {
            ValidationEnd::Failed(format!("the validation run could not be reaped: {error}"))
        }
        AgentRunOutcome::Unsettled => {
            ValidationEnd::Failed("the validation run left an unsettled process group".to_owned())
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::{ValidationEnd, end_of};
    use crate::agent_run::bounded::AgentRunOutcome;
    use std::os::unix::process::ExitStatusExt;

    #[test]
    fn bounded_outcomes_map_to_portable_ends() {
        let exited =
            |code: i32| AgentRunOutcome::Exited(std::process::ExitStatus::from_raw(code << 8));
        assert_eq!(end_of(&exited(0)), ValidationEnd::Exited(Some(0)));
        assert_eq!(end_of(&exited(125)), ValidationEnd::Exited(Some(125)));
        assert_eq!(
            end_of(&AgentRunOutcome::Exited(
                std::process::ExitStatus::from_raw(9)
            )),
            ValidationEnd::Exited(None),
            "a signal death has no exit code"
        );
        assert_eq!(end_of(&AgentRunOutcome::TimedOut), ValidationEnd::TimedOut);
        assert_eq!(
            end_of(&AgentRunOutcome::Cancelled),
            ValidationEnd::Cancelled
        );
        assert!(matches!(
            end_of(&AgentRunOutcome::Unsettled),
            ValidationEnd::Failed(_)
        ));
        assert!(matches!(
            end_of(&AgentRunOutcome::Spawn(std::io::Error::other("x"))),
            ValidationEnd::Failed(why) if why.contains("spawn")
        ));
    }
}
