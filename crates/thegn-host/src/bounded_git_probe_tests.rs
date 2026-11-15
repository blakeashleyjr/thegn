use super::*;
use std::process::ExitStatus;
use thegn_proc::{Capture, Containment, Which};

#[test]
fn unfinished_child_or_pipe_owns_capacity_until_all_holders_release() {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let child = git_budget(&COUNTER).unwrap();
    let stuck_reader = Arc::clone(&child);
    let second = git_budget(&COUNTER).unwrap();
    assert!(git_budget(&COUNTER).is_err());
    drop(child);
    assert!(
        git_budget(&COUNTER).is_err(),
        "reader still owns first slot"
    );
    drop(stuck_reader);
    let replacement = git_budget(&COUNTER).unwrap();
    drop(second);
    drop(replacement);
    assert_eq!(COUNTER.load(Ordering::Acquire), 0);
}

#[test]
fn capability_fixed_policy_refuses_invalid_limits_before_spawn() {
    let dir = tempfile::tempdir().unwrap();
    for (duration, bytes, expected) in [
        (std::time::Duration::ZERO, 16, "invalid capture deadline"),
        (
            std::time::Duration::from_secs(6),
            16,
            "capability capture exceeds its fixed safety policy",
        ),
        (
            std::time::Duration::from_secs(1),
            0,
            "invalid capture byte bound",
        ),
        (
            std::time::Duration::from_secs(1),
            16 * 1024 + 1,
            "capability capture exceeds its fixed safety policy",
        ),
    ] {
        // Nonexistent command distinguishes admission refusal from spawn error.
        let command = std::process::Command::new(dir.path().join("must-not-spawn"));
        let error = capture_capability(command, duration, bytes).unwrap_err();
        assert_eq!(error.to_string(), expected);
    }
}

#[test]
fn capability_budget_is_independent_of_git_budget() {
    static GIT: AtomicUsize = AtomicUsize::new(0);
    static CAPABILITY: AtomicUsize = AtomicUsize::new(0);
    let capability = budget_for(&CAPABILITY, CaptureLane::Capability).unwrap();
    let refused = budget_for(&CAPABILITY, CaptureLane::Capability).unwrap_err();
    assert_eq!(
        refused.to_string(),
        "capability capacity held by unfinished child/pipe; retry later"
    );
    let first = budget_for(&GIT, CaptureLane::Git).unwrap();
    let second = budget_for(&GIT, CaptureLane::Git).unwrap();
    let refused = budget_for(&GIT, CaptureLane::Git).unwrap_err();
    assert_eq!(
        refused.to_string(),
        "cleanup Git capacity held by unfinished child/pipe; retry later"
    );
    drop((first, second, capability));
    assert_eq!(GIT.load(Ordering::Acquire), 0);
    assert_eq!(CAPABILITY.load(Ordering::Acquire), 0);
}

#[test]
fn spawn_failure_releases_its_slot() {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let dir = tempfile::tempdir().unwrap();
    let command = std::process::Command::new(dir.path().join("missing-probe"));
    let error = capture_output_with(
        command,
        None,
        std::time::Duration::from_secs(1),
        16,
        &COUNTER,
        CaptureLane::Capability,
    )
    .unwrap_err();
    assert!(
        error
            .to_string()
            .starts_with("capability probe could not start: "),
        "{error}"
    );
    assert_eq!(COUNTER.load(Ordering::Acquire), 0);
}

fn fake_run(end: End, group: GroupState, leader: Leader) -> Run {
    Run {
        end,
        group,
        leader,
        leader_exited_first: true,
        stdout: Capture {
            bytes: b"out".to_vec(),
            eof: true,
            ..Capture::default()
        },
        stderr: Capture {
            bytes: b"err".to_vec(),
            eof: true,
            ..Capture::default()
        },
        containment: Containment::ProcessGroup,
    }
}

fn reaped() -> Leader {
    Leader::Reaped(ExitStatus::default())
}

fn mapped_error(run: Run, feed: Option<InputFeed>) -> String {
    map_capture(run, feed, "Git").unwrap_err().to_string()
}

#[test]
fn map_capture_reports_each_end_with_fixed_messages() {
    let cases = [
        (
            fake_run(
                End::Spawn(std::io::Error::other("no such tool")),
                GroupState::Unknown,
                Leader::NotSpawned,
            ),
            "Git could not start: no such tool",
        ),
        (
            fake_run(End::TimedOut, GroupState::Terminated, reaped()),
            "Git operation exceeded cleanup deadline",
        ),
        (
            fake_run(
                End::OutputLimit(Which::Stdout),
                GroupState::Terminated,
                reaped(),
            ),
            "Git probe output exceeded the safety bound",
        ),
        (
            fake_run(
                End::OutputLimit(Which::Stderr),
                GroupState::Terminated,
                reaped(),
            ),
            "Git probe output exceeded the safety bound",
        ),
        (
            fake_run(End::Cancelled, GroupState::Terminated, reaped()),
            "Git wait failed: cancelled",
        ),
        (
            fake_run(
                End::Supervise(std::io::Error::other("waitid")),
                GroupState::Unknown,
                Leader::ReapFailed(std::io::Error::other("waitid")),
            ),
            "Git wait failed: waitid",
        ),
        (
            fake_run(End::LeaderExited, GroupState::Survivors, Leader::Detached),
            "Git wait failed",
        ),
        (
            fake_run(
                End::LeaderExited,
                GroupState::Drained,
                Leader::ReapFailed(std::io::Error::other("ECHILD")),
            ),
            "Git wait failed: ECHILD",
        ),
    ];
    for (run, expected) in cases {
        assert_eq!(mapped_error(run, None), expected);
    }
}

#[test]
fn map_capture_timeout_keeps_the_merge_cleanup_substring() {
    // merge_cleanup.rs matches this text to classify an OCI probe timeout.
    let message = mapped_error(
        fake_run(End::TimedOut, GroupState::Terminated, reaped()),
        None,
    );
    assert!(message.contains("exceeded cleanup deadline"), "{message}");
}

#[test]
fn map_capture_precedence_prefers_the_end_over_the_feed() {
    let broken = Some(InputFeed {
        written: 0,
        complete: false,
        error: Some(std::io::ErrorKind::BrokenPipe),
    });
    assert_eq!(
        mapped_error(
            fake_run(End::TimedOut, GroupState::Terminated, reaped()),
            broken
        ),
        "Git operation exceeded cleanup deadline"
    );
    assert_eq!(
        mapped_error(
            fake_run(End::LeaderExited, GroupState::Survivors, Leader::Detached),
            broken
        ),
        "Git wait failed"
    );
}

#[test]
fn map_capture_reports_an_incomplete_input_feed() {
    let message = mapped_error(
        fake_run(End::LeaderExited, GroupState::Drained, reaped()),
        Some(InputFeed {
            written: 3,
            complete: false,
            error: Some(std::io::ErrorKind::BrokenPipe),
        }),
    );
    assert!(message.starts_with("Git input failed: "), "{message}");
}

#[test]
fn map_capture_returns_status_and_both_streams_after_a_reaped_exit() {
    for feed in [
        None,
        Some(InputFeed {
            written: 5,
            complete: true,
            error: None,
        }),
        // The run ended before the feed finished, with no write error.
        Some(InputFeed::default()),
    ] {
        let output = map_capture(
            fake_run(End::LeaderExited, GroupState::Drained, reaped()),
            feed,
            "capability probe",
        )
        .unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"out");
        assert_eq!(output.stderr, b"err");
    }
}

#[test]
fn budget_is_released_only_when_nothing_can_outlive_the_capture() {
    let cases = [
        (
            fake_run(End::LeaderExited, GroupState::Drained, reaped()),
            true,
        ),
        (
            fake_run(End::TimedOut, GroupState::Terminated, reaped()),
            true,
        ),
        (
            fake_run(
                End::Spawn(std::io::Error::other("spawn")),
                GroupState::Unknown,
                Leader::NotSpawned,
            ),
            true,
        ),
        (
            fake_run(End::LeaderExited, GroupState::Survivors, reaped()),
            false,
        ),
        (
            fake_run(
                End::Supervise(std::io::Error::other("waitid")),
                GroupState::Unknown,
                reaped(),
            ),
            false,
        ),
        (
            fake_run(End::TimedOut, GroupState::Terminated, Leader::Detached),
            false,
        ),
        (
            fake_run(
                End::LeaderExited,
                GroupState::Drained,
                Leader::ReapFailed(std::io::Error::other("ECHILD")),
            ),
            false,
        ),
        (
            fake_run(End::LeaderExited, GroupState::Drained, Leader::NotSpawned),
            false,
        ),
    ];
    for (run, expected) in cases {
        assert_eq!(releases_budget(&run), expected, "{run:?}");
    }
}

#[path = "platform/bounded_capability_tests.rs"]
mod capability_process_tests;
