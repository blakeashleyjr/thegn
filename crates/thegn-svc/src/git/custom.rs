//! User-defined git commands (`[[git_commands]]`): the host collects prompt
//! responses, thegn-core's `custom_cmd::compile` admits the template
//! against the current selection, and this runs the result in the worktree.
//! `terminal` output mode is executed by the host's floating-pane machinery
//! instead — this seam only covers capture (`popup`) and fire-and-forget.

use super::GitBackend;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use thegn_core::remote::GitLoc;
use thegn_proc::{End, GroupState, Leader, Limits, Stream, Which};

pub const CUSTOM_COMMAND_TIMEOUT_SECS: u64 = thegn_core::custom_cmd::DEFAULT_TIMEOUT_SECS;
const CUSTOM_COMMAND_TIMEOUT: Duration = Duration::from_secs(CUSTOM_COMMAND_TIMEOUT_SECS);
const STDOUT_LIMIT: usize = 1024 * 1024;
const STDERR_LIMIT: usize = 64 * 1024;
const READER_GRACE: Duration = Duration::from_secs(2);
pub const CUSTOM_OUTCOME_EXITED: &str = "exited";
pub const CUSTOM_OUTCOME_TIMEOUT: &str = "timeout";
pub const CUSTOM_OUTCOME_SPAWN: &str = "spawn";
pub const CUSTOM_OUTCOME_WAIT: &str = "wait";
pub const CUSTOM_OUTCOME_READ: &str = "read";
pub const CUSTOM_OUTCOME_CLEANUP: &str = "cleanup";
pub const CUSTOM_OUTCOME_CANCELLED: &str = "cancelled";

/// Bounded result from a custom command. Stderr is retained only so the
/// non-template command path can preserve its historical failure summary.
#[derive(Debug)]
enum CustomOutcome {
    Exited {
        status: ExitStatus,
        stdout: String,
        stdout_truncated: bool,
        stderr: String,
    },
    Timeout,
    Spawn,
    Wait,
    Read,
    IncompleteCleanup,
    Cancelled,
}

impl CustomOutcome {
    fn into_parts(self) -> (&'static str, Option<ExitStatus>, String, bool, String) {
        match self {
            Self::Exited {
                status,
                stdout,
                stdout_truncated,
                stderr,
            } => (
                CUSTOM_OUTCOME_EXITED,
                Some(status),
                stdout,
                stdout_truncated,
                stderr,
            ),
            Self::Timeout => (
                CUSTOM_OUTCOME_TIMEOUT,
                None,
                String::new(),
                false,
                String::new(),
            ),
            Self::Spawn => (
                CUSTOM_OUTCOME_SPAWN,
                None,
                String::new(),
                false,
                String::new(),
            ),
            Self::Wait => (
                CUSTOM_OUTCOME_WAIT,
                None,
                String::new(),
                false,
                String::new(),
            ),
            Self::Read => (
                CUSTOM_OUTCOME_READ,
                None,
                String::new(),
                false,
                String::new(),
            ),
            Self::IncompleteCleanup => (
                CUSTOM_OUTCOME_CLEANUP,
                None,
                String::new(),
                false,
                String::new(),
            ),
            Self::Cancelled => (
                CUSTOM_OUTCOME_CANCELLED,
                None,
                String::new(),
                false,
                String::new(),
            ),
        }
    }
}

pub trait CustomOps: GitBackend {
    /// Run a command admitted by the custom-command compiler.
    fn run_custom(
        &self,
        loc: &GitLoc,
        command: &str,
        capture: bool,
        cancel: &AtomicBool,
    ) -> (&'static str, Option<ExitStatus>, String, bool, String) {
        let mut command = loc.sh_command(command);
        command.stdin(Stdio::null()).env("GIT_TERMINAL_PROMPT", "0");
        run_bounded(
            command,
            capture,
            CUSTOM_COMMAND_TIMEOUT,
            STDOUT_LIMIT,
            STDERR_LIMIT,
            cancel,
        )
        .into_parts()
    }

    /// Execute only a command admitted by the custom-template compiler.
    fn run_custom_template(
        &self,
        loc: &GitLoc,
        command: &thegn_core::custom_cmd::ExpandedCommand,
        capture: bool,
        cancel: &AtomicBool,
    ) -> (&'static str, Option<ExitStatus>, String, bool, String) {
        let timeout = command.timeout();
        let mut command = command.command(loc);
        command.stdin(Stdio::null()).env("GIT_TERMINAL_PROMPT", "0");
        run_bounded(
            command,
            capture,
            timeout,
            STDOUT_LIMIT,
            STDERR_LIMIT,
            cancel,
        )
        .into_parts()
    }
}

impl<T: GitBackend + ?Sized> CustomOps for T {}

fn run_bounded(
    mut command: Command,
    capture: bool,
    timeout: Duration,
    stdout_limit: usize,
    stderr_limit: usize,
    cancel: &AtomicBool,
) -> CustomOutcome {
    if cancel.load(Ordering::Relaxed) {
        return CustomOutcome::Cancelled;
    }
    let limits = Limits {
        quiesce_after_leader: Some(Duration::ZERO),
        reader_grace: READER_GRACE,
        stdout: if capture {
            Stream::Head { cap: stdout_limit }
        } else {
            Stream::Null
        },
        stderr: Stream::Tail { cap: stderr_limit },
        ..Limits::silent(timeout)
    };
    outcome(
        thegn_proc::run(&mut command, &limits, cancel),
        capture,
        stdout_limit,
        stderr_limit,
    )
}

fn outcome(
    run: thegn_proc::Run,
    capture: bool,
    stdout_limit: usize,
    stderr_limit: usize,
) -> CustomOutcome {
    if matches!(&run.end, End::Spawn(_)) {
        return CustomOutcome::Spawn;
    }
    if matches!(&run.end, End::Supervise(_)) || matches!(&run.leader, Leader::ReapFailed(_)) {
        return CustomOutcome::Wait;
    }
    if run.group == GroupState::Survivors || matches!(&run.leader, Leader::Detached) {
        return CustomOutcome::IncompleteCleanup;
    }
    match &run.end {
        End::Cancelled => CustomOutcome::Cancelled,
        End::TimedOut => CustomOutcome::Timeout,
        End::OutputLimit(Which::Stdout | Which::Stderr) => CustomOutcome::Read,
        End::LeaderExited => match &run.leader {
            Leader::Reaped(status) => {
                if (capture && !run.stdout.eof) || !run.stderr.eof {
                    return CustomOutcome::IncompleteCleanup;
                }
                let (stdout, decode_truncated) =
                    bounded_lossy_utf8(&run.stdout.bytes, stdout_limit);
                let (stderr, _) = bounded_lossy_utf8(&run.stderr.bytes, stderr_limit);
                CustomOutcome::Exited {
                    status: *status,
                    stdout,
                    stdout_truncated: run.stdout.truncated || decode_truncated,
                    stderr,
                }
            }
            _ => CustomOutcome::Wait,
        },
        End::Spawn(_) | End::Supervise(_) => unreachable!("handled above"),
    }
}

fn bounded_lossy_utf8(bytes: &[u8], limit: usize) -> (String, bool) {
    let mut output = String::with_capacity(bytes.len().min(limit));
    let mut remaining = bytes;
    while !remaining.is_empty() {
        match std::str::from_utf8(remaining) {
            Ok(valid) => {
                let room = limit.saturating_sub(output.len());
                let mut end = valid.len().min(room);
                while !valid.is_char_boundary(end) {
                    end -= 1;
                }
                output.push_str(&valid[..end]);
                return (output, end < valid.len());
            }
            Err(error) => {
                let valid_len = error.valid_up_to();
                let valid = std::str::from_utf8(&remaining[..valid_len])
                    .expect("valid_up_to is a UTF-8 boundary");
                let room = limit.saturating_sub(output.len());
                if valid.len() > room {
                    let mut end = room;
                    while !valid.is_char_boundary(end) {
                        end -= 1;
                    }
                    output.push_str(&valid[..end]);
                    return (output, true);
                }
                output.push_str(valid);
                if limit.saturating_sub(output.len()) < '\u{fffd}'.len_utf8() {
                    return (output, true);
                }
                output.push('\u{fffd}');
                let invalid_len = error.error_len().unwrap_or(remaining.len() - valid_len);
                remaining = &remaining[valid_len + invalid_len..];
            }
        }
    }
    (output, false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::{CliGit, testutil::TestRepo};
    use std::{sync::Arc, time::Instant};

    fn supported() -> bool {
        crate::plugin::proc::bounded_group_termination_supported()
    }

    #[test]
    fn reaps_descendants_that_keep_pipes_open_after_leader_exit() {
        if !supported() {
            return;
        }
        let repo = TestRepo::new("custom-descendant");
        let started = Instant::now();
        let outcome = CliGit.run_custom(
            &repo.loc(),
            "(sleep 30; echo late) & echo done",
            true,
            &AtomicBool::new(false),
        );
        assert!(started.elapsed() < Duration::from_secs(5));
        let (kind, status, stdout, _, _) = outcome;
        assert_eq!(kind, CUSTOM_OUTCOME_EXITED);
        assert!(status.is_some_and(|status| status.success()));
        assert_eq!(stdout.trim(), "done");
    }

    #[test]
    fn popup_capture_is_bounded_and_reports_truncation() {
        if !supported() {
            return;
        }
        let repo = TestRepo::new("custom-output-cap");
        match run_bounded(
            repo.loc().sh_command("head -c 5000000 /dev/zero"),
            true,
            Duration::from_secs(5),
            1024 * 1024,
            STDERR_LIMIT,
            &AtomicBool::new(false),
        ) {
            CustomOutcome::Exited {
                stdout,
                stdout_truncated,
                ..
            } => {
                assert!(stdout.len() <= 1024 * 1024);
                assert!(stdout_truncated);
            }
            other => panic!("expected bounded command output, got {other:?}"),
        }
    }

    #[test]
    fn outcome_mapping_precedence_and_incomplete_pipe_are_preserved() {
        let empty = thegn_proc::Capture::default();
        let cancelled = thegn_proc::Run {
            end: End::Cancelled,
            group: GroupState::Survivors,
            leader: Leader::Detached,
            leader_exited_first: false,
            stdout: empty.clone(),
            stderr: empty.clone(),
            containment: thegn_proc::Containment::ProcessGroup,
        };
        assert!(matches!(
            outcome(cancelled, true, STDOUT_LIMIT, STDERR_LIMIT),
            CustomOutcome::IncompleteCleanup
        ));

        let supervised = thegn_proc::Run {
            end: End::Supervise(std::io::Error::other("fixture")),
            group: GroupState::Unknown,
            leader: Leader::NotSpawned,
            leader_exited_first: false,
            stdout: empty.clone(),
            stderr: empty.clone(),
            containment: thegn_proc::Containment::ProcessGroup,
        };
        assert!(matches!(
            outcome(supervised, true, STDOUT_LIMIT, STDERR_LIMIT),
            CustomOutcome::Wait
        ));

        let limited = thegn_proc::Run {
            end: End::OutputLimit(Which::Stdout),
            group: GroupState::Terminated,
            leader: Leader::NotSpawned,
            leader_exited_first: false,
            stdout: empty.clone(),
            stderr: empty.clone(),
            containment: thegn_proc::Containment::ProcessGroup,
        };
        assert!(matches!(
            outcome(limited, true, STDOUT_LIMIT, STDERR_LIMIT),
            CustomOutcome::Read
        ));

        let repo = TestRepo::new("custom-outcome-status");
        let status = match run_bounded(
            repo.loc().sh_command("true"),
            true,
            Duration::from_secs(2),
            STDOUT_LIMIT,
            STDERR_LIMIT,
            &AtomicBool::new(false),
        ) {
            CustomOutcome::Exited { status, .. } => status,
            other => panic!("expected successful fixture command, got {other:?}"),
        };
        let escaped_pipe = thegn_proc::Run {
            end: End::LeaderExited,
            group: GroupState::Drained,
            leader: Leader::Reaped(status),
            leader_exited_first: true,
            stdout: thegn_proc::Capture {
                eof: false,
                ..empty.clone()
            },
            stderr: empty,
            containment: thegn_proc::Containment::ProcessGroup,
        };
        assert!(matches!(
            outcome(escaped_pipe, true, STDOUT_LIMIT, STDERR_LIMIT),
            CustomOutcome::IncompleteCleanup
        ));
    }

    #[test]
    fn repeated_timeout_and_cancel_cycles_leave_no_descendant() {
        if !supported() {
            return;
        }
        let repo = TestRepo::new("custom-repeat-cleanup");
        let markers: Vec<_> = (0..6)
            .map(|index| repo.dir.join(format!("marker-{index}")))
            .collect();
        for (index, marker) in markers.iter().enumerate() {
            let script = format!("(sleep 1; touch '{}') & sleep 30", marker.display());
            if index % 2 == 0 {
                let started = Instant::now();
                assert!(matches!(
                    run_bounded(
                        repo.loc().sh_command(&script),
                        true,
                        Duration::from_millis(200),
                        STDOUT_LIMIT,
                        STDERR_LIMIT,
                        &AtomicBool::new(false),
                    ),
                    CustomOutcome::Timeout
                ));
                assert!(started.elapsed() < Duration::from_secs(3));
            } else {
                let command = repo.loc().sh_command(&script);
                let cancel = Arc::new(AtomicBool::new(false));
                let worker_cancel = Arc::clone(&cancel);
                let worker = std::thread::spawn(move || {
                    run_bounded(
                        command,
                        true,
                        Duration::from_secs(5),
                        STDOUT_LIMIT,
                        STDERR_LIMIT,
                        &worker_cancel,
                    )
                });
                std::thread::sleep(Duration::from_millis(100));
                cancel.store(true, Ordering::Relaxed);
                assert!(matches!(
                    worker.join().expect("cancelled command worker"),
                    CustomOutcome::Cancelled
                ));
            }
        }
        std::thread::sleep(Duration::from_millis(1200));
        assert!(markers.iter().all(|marker| !marker.exists()));
        let (kind, status, stdout, _, _) =
            CliGit.run_custom(&repo.loc(), "echo ok", true, &AtomicBool::new(false));
        assert_eq!(kind, CUSTOM_OUTCOME_EXITED);
        assert!(status.is_some_and(|status| status.success()));
        assert_eq!(stdout.trim(), "ok");
    }

    #[test]
    fn none_mode_discards_output_body() {
        if !supported() {
            return;
        }
        let repo = TestRepo::new("custom-none-output");
        match run_bounded(
            repo.loc()
                .sh_command("printf 'output that must be discarded'"),
            false,
            Duration::from_secs(5),
            STDOUT_LIMIT,
            STDERR_LIMIT,
            &AtomicBool::new(false),
        ) {
            CustomOutcome::Exited { stdout, .. } => assert!(stdout.is_empty()),
            other => panic!("expected command completion without output body, got {other:?}"),
        }
    }

    #[test]
    fn timeout_kills_descendant_and_next_command_runs() {
        if !supported() {
            return;
        }
        let repo = TestRepo::new("custom-timeout");
        let marker = repo.dir.join("marker");
        let script = format!("(sleep 1; touch '{}') & sleep 30", marker.display());
        match run_bounded(
            repo.loc().sh_command(&script),
            true,
            Duration::from_millis(300),
            STDOUT_LIMIT,
            STDERR_LIMIT,
            &AtomicBool::new(false),
        ) {
            CustomOutcome::Timeout => {}
            other => panic!("expected timeout, got {other:?}"),
        }
        std::thread::sleep(Duration::from_secs(2));
        assert!(!marker.exists(), "descendant survived group termination");
        let (kind, status, stdout, _, _) =
            CliGit.run_custom(&repo.loc(), "echo ok", true, &AtomicBool::new(false));
        assert_eq!(kind, CUSTOM_OUTCOME_EXITED);
        assert!(status.is_some_and(|status| status.success()));
        assert_eq!(stdout.trim(), "ok");
    }

    #[test]
    fn cancellation_kills_descendant_and_next_command_runs() {
        if !supported() {
            return;
        }
        let repo = TestRepo::new("custom-cancel");
        let marker = repo.dir.join("cancelled-descendant");
        let script = format!("(sleep 1; touch '{}') & sleep 30", marker.display());
        let command = repo.loc().sh_command(&script);
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = Arc::clone(&cancel);
        let worker = std::thread::spawn(move || {
            run_bounded(
                command,
                true,
                Duration::from_secs(20),
                STDOUT_LIMIT,
                STDERR_LIMIT,
                &worker_cancel,
            )
        });
        std::thread::sleep(Duration::from_millis(100));
        cancel.store(true, Ordering::Relaxed);
        assert!(matches!(
            worker.join().expect("command worker"),
            CustomOutcome::Cancelled
        ));
        std::thread::sleep(Duration::from_millis(1100));
        assert!(
            !marker.exists(),
            "cancelled descendant survived group termination"
        );
        let (kind, status, stdout, _, _) =
            CliGit.run_custom(&repo.loc(), "echo ok", true, &AtomicBool::new(false));
        assert_eq!(kind, CUSTOM_OUTCOME_EXITED);
        assert!(status.is_some_and(|status| status.success()));
        assert_eq!(stdout.trim(), "ok");
    }

    #[test]
    fn pre_cancelled_command_does_not_spawn() {
        if !supported() {
            return;
        }
        let repo = TestRepo::new("custom-pre-cancel");
        let marker = repo.dir.join("must-not-exist");
        let script = format!("touch '{}'", marker.display());
        let cancel = AtomicBool::new(true);
        let outcome = run_bounded(
            repo.loc().sh_command(&script),
            true,
            Duration::from_secs(5),
            STDOUT_LIMIT,
            STDERR_LIMIT,
            &cancel,
        );
        assert!(matches!(outcome, CustomOutcome::Cancelled));
        assert!(!marker.exists());
    }
}
