//! Merge-queue gate verdict classification.
//!
//! The fold's test gate answers one question — "does the folded tip build?" —
//! but a gate command can fail for two categorically different reasons, and
//! conflating them is a correctness bug, not a cosmetic one:
//!
//!  * the command **ran** and exited non-zero — a verdict about the *code*;
//!  * the command **could not run** at all (missing binary, non-executable,
//!    killed by a signal, unprovisioned worktree) — a fact about the
//!    *environment*, which says nothing about the branch.
//!
//! Only the first is a reason to blame a branch, hand it to a fixing agent, or
//! bisect for an offender. An environment failure reproduces identically at
//! every commit, so bisecting one burns a full gate run per prefix and then
//! blames an arbitrary branch; dispatching an agent on one sets a coding model
//! loose on source code in response to `command not found`.
//!
//! Kept pure and substrate-free so the classification is unit-tested rather
//! than inferred from a live gate run.

/// What a gate invocation actually established.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateClass {
    /// The command ran and exited zero.
    Passed,
    /// The command ran and exited non-zero — a verdict about the code.
    Failed,
    /// The command could not run — a fact about the environment.
    Error,
}

/// Shell exit code for "command not found".
pub const EXIT_NOT_FOUND: i32 = 127;
/// Shell exit code for "found, but not executable".
pub const EXIT_NOT_EXECUTABLE: i32 = 126;

/// Classify one gate invocation.
///
/// `code` is the process exit status: `Some(n)` for a normal exit, `None` when
/// the process was killed by a signal (which is an environment fact — an OOM
/// kill or a timeout, never the branch's verdict). `spawn_failed` is true when
/// the command could not be launched at all.
///
/// 127/126 come from the shell, not from the gate command, so they are the one
/// reliable in-band signal that the command never ran. The same rule is already
/// applied to remote exec (see `transport_error::classify_exec`).
pub fn classify_exit(code: Option<i32>, spawn_failed: bool) -> GateClass {
    if spawn_failed {
        return GateClass::Error;
    }
    match code {
        Some(0) => GateClass::Passed,
        // Killed by a signal: the environment ended it, not the code.
        None => GateClass::Error,
        Some(EXIT_NOT_FOUND | EXIT_NOT_EXECUTABLE) => GateClass::Error,
        Some(_) => GateClass::Failed,
    }
}

/// A short, human reason for a [`GateClass::Error`], used as the queue row's
/// `error_detail` headline so `merge list` says what actually happened instead
/// of blaming the branch.
pub fn error_reason(code: Option<i32>, spawn_failed: bool) -> &'static str {
    if spawn_failed {
        return "gate command could not be started";
    }
    match code {
        None => "gate command was killed by a signal",
        Some(EXIT_NOT_FOUND) => "gate command not found (exit 127)",
        Some(EXIT_NOT_EXECUTABLE) => "gate command is not executable (exit 126)",
        // Not an error class; kept total so callers can't construct a wrong message.
        Some(_) => "gate environment failure",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_passes_and_ordinary_failures_are_code_verdicts() {
        assert_eq!(classify_exit(Some(0), false), GateClass::Passed);
        for code in [1, 2, 3, 42, 101, 125, 128, 255] {
            assert_eq!(
                classify_exit(Some(code), false),
                GateClass::Failed,
                "exit {code} should be a verdict about the code"
            );
        }
    }

    #[test]
    fn unrunnable_commands_are_environment_errors() {
        // The two shell "never ran it" codes.
        assert_eq!(classify_exit(Some(127), false), GateClass::Error);
        assert_eq!(classify_exit(Some(126), false), GateClass::Error);
        // Killed by a signal (OOM, watchdog).
        assert_eq!(classify_exit(None, false), GateClass::Error);
        // Could not even spawn the shell.
        assert_eq!(classify_exit(None, true), GateClass::Error);
        // spawn_failed wins even when a code is somehow present.
        assert_eq!(classify_exit(Some(0), true), GateClass::Error);
        assert_eq!(classify_exit(Some(1), true), GateClass::Error);
    }

    #[test]
    fn error_reasons_name_the_actual_cause() {
        assert!(error_reason(None, true).contains("could not be started"));
        assert!(error_reason(None, false).contains("signal"));
        assert!(error_reason(Some(127), false).contains("not found"));
        assert!(error_reason(Some(126), false).contains("not executable"));
        // Total for the non-error codes rather than panicking.
        assert!(!error_reason(Some(1), false).is_empty());
    }
}
