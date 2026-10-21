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

/// A route that runs candidate-controlled code on the host until THE-794's
/// admission handle exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostExecutionRoute {
    /// Stable route identifier used by doctor output.
    pub id: &'static str,
    /// The candidate-controlled code that runs.
    pub runs: &'static str,
    /// The configuration key or action that selects the route.
    pub selected_by: &'static str,
}

/// Every route that runs candidate-controlled code on the host until
/// THE-794's admission handle exists.
pub const HOST_EXECUTION_ROUTES: &[HostExecutionRoute] = &[
    HostExecutionRoute {
        id: "gate_setup",
        runs: "the candidate's setup command, through sh -c",
        selected_by: "[merge_queue] gate_setup_command",
    },
    HostExecutionRoute {
        id: "gate",
        runs: "the candidate's gate command, through sh -c",
        selected_by: "[merge_queue] gate_command",
    },
    HostExecutionRoute {
        id: "regenerate",
        runs: "the candidate's regenerate command during a fold",
        selected_by: "[merge_queue] regenerate_command",
    },
    HostExecutionRoute {
        id: "merge_drivers",
        runs: "custom git merge drivers and rerere during a fold",
        selected_by: "custom git merge drivers and rerere during a fold",
    },
    HostExecutionRoute {
        id: "tests_panel",
        runs: "the selected test command from the Tests panel",
        selected_by: "Tests panel runs",
    },
    HostExecutionRoute {
        id: "test_discovery",
        runs: "the configured command used to discover tests",
        selected_by: "command-based test discovery",
    },
];

/// Shared user-facing label for candidate code that runs without isolation.
pub const HOST_EXECUTION_LABEL: &str = "host execution (no isolation)";

/// Header prepended to every merge-gate failure or error log.
pub const HOST_EXECUTION_LOG_HEADER: &str = "[thegn] this gate ran as host execution (no isolation): candidate code had this user's environment, files and network\n";

/// JSON doctor representation of every host-executed candidate-code route.
pub fn host_execution_json() -> serde_json::Value {
    serde_json::Value::Array(
        HOST_EXECUTION_ROUTES
            .iter()
            .map(|route| {
                serde_json::json!({
                    "route": route.id,
                    "runs": route.runs,
                    "selected_by": route.selected_by,
                    "isolation": "none",
                })
            })
            .collect(),
    )
}

/// Human-readable doctor lines for every host-executed candidate-code route.
pub fn host_execution_lines() -> Vec<String> {
    let mut lines = Vec::with_capacity(HOST_EXECUTION_ROUTES.len() + 2);
    lines.push("Host execution (no isolation)".into());
    lines.extend(
        HOST_EXECUTION_ROUTES
            .iter()
            .map(|route| format!("  {}  {}: {}", route.id, route.selected_by, route.runs)),
    );
    lines.push("THE-794's admission handle is the planned isolated-runner fix.".into());
    lines
}

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

/// Terminal event reported by an owned gate runner after setup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunEnd {
    /// The started entry point exited with its own numeric status.
    Exited(i32),
    /// The entry point was terminated by a signal.
    Signaled,
    /// Runner deadline expired.
    RunnerTimeout,
    /// Runner terminated the process for a resource or policy limit.
    RunnerKilled,
    /// Runner transport was lost before a terminal event arrived.
    TransportLost,
    /// No trustworthy terminal event was available.
    Unknown,
}

/// Classify authenticated runner evidence. A number is a code verdict only
/// after the runner confirms that the requested entry point started.
pub fn classify_run(started: bool, end: RunEnd) -> GateClass {
    if !started {
        return GateClass::Error;
    }
    match end {
        RunEnd::Exited(0) => GateClass::Passed,
        RunEnd::Exited(_) => GateClass::Failed,
        RunEnd::Signaled
        | RunEnd::RunnerTimeout
        | RunEnd::RunnerKilled
        | RunEnd::TransportLost
        | RunEnd::Unknown => GateClass::Error,
    }
}

/// Short reason for runner evidence classified as an infrastructure error.
pub fn run_error_reason(started: bool, end: RunEnd) -> &'static str {
    if !started {
        return "gate entry point did not start";
    }
    match end {
        RunEnd::Signaled => "gate entry point was killed by a signal",
        RunEnd::RunnerTimeout => "gate runner timed out",
        RunEnd::RunnerKilled => "gate runner killed the entry point",
        RunEnd::TransportLost => "gate runner transport was lost",
        RunEnd::Unknown => "gate runner reported no trustworthy terminal event",
        RunEnd::Exited(_) => "gate command exited normally",
    }
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

    #[test]
    fn owned_runner_evidence_separates_code_exits_from_runner_failures() {
        for code in [125, 126, 127, 137, 255] {
            assert_eq!(classify_run(true, RunEnd::Exited(code)), GateClass::Failed);
        }
        assert_eq!(classify_run(true, RunEnd::Exited(0)), GateClass::Passed);
        assert_eq!(classify_run(false, RunEnd::Exited(0)), GateClass::Error);
        for end in [
            RunEnd::Signaled,
            RunEnd::RunnerTimeout,
            RunEnd::RunnerKilled,
            RunEnd::TransportLost,
            RunEnd::Unknown,
        ] {
            assert_eq!(classify_run(true, end), GateClass::Error);
            assert!(!run_error_reason(true, end).is_empty());
        }
        assert!(!run_error_reason(true, RunEnd::Exited(1)).is_empty());
        assert!(run_error_reason(false, RunEnd::Unknown).contains("did not start"));
    }

    #[test]
    fn host_execution_routes_name_every_candidate_code_route() {
        let ids: Vec<_> = HOST_EXECUTION_ROUTES.iter().map(|route| route.id).collect();
        assert_eq!(
            ids,
            [
                "gate_setup",
                "gate",
                "regenerate",
                "merge_drivers",
                "tests_panel",
                "test_discovery",
            ]
        );
        for (index, route) in HOST_EXECUTION_ROUTES.iter().enumerate() {
            assert!(
                !route.runs.is_empty(),
                "{} has no runs description",
                route.id
            );
            assert!(
                !route.selected_by.is_empty(),
                "{} has no selector description",
                route.id
            );
            assert!(
                !ids[..index].contains(&route.id),
                "{} is duplicated",
                route.id
            );
        }
    }

    #[test]
    fn host_execution_json_marks_every_route_unisolated() {
        let value = host_execution_json();
        let entries = value.as_array().expect("route list should be an array");
        assert_eq!(entries.len(), HOST_EXECUTION_ROUTES.len());
        for (entry, route) in entries.iter().zip(HOST_EXECUTION_ROUTES) {
            assert_eq!(entry["route"], route.id);
            assert_eq!(entry["isolation"], "none");
        }
    }

    #[test]
    fn host_execution_lines_start_with_the_label_heading() {
        let lines = host_execution_lines();
        assert!(lines[0].contains("Host execution (no isolation)"));
        for route in HOST_EXECUTION_ROUTES {
            assert_eq!(
                lines
                    .iter()
                    .filter(|line| line.starts_with(&format!("  {}  ", route.id)))
                    .count(),
                1,
                "missing doctor line for {}",
                route.id
            );
        }
    }
}
