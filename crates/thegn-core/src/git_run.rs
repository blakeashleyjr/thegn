//! Typed, bounded Git execution for THE-368 slice a.
//!
//! This introduces the executor without migrating any existing callers. Git reads
//! are complete when the leader exits; a helper that keeps a pipe open is quiesced
//! immediately. Calls block, so `git_run` must run off the UI loop.

use std::{
    fmt,
    path::Path,
    process::{Command, ExitStatus},
    sync::atomic::AtomicBool,
    time::Duration,
};

use thegn_proc::{End, Leader, Limits, Stream, Which};

/// Per-call execution and capture budgets. The defaults in [`GitPolicy::read`]
/// are starting points for callers to override; they are not a global limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GitPolicy {
    pub deadline: Duration,
    pub stdout_cap: usize,
    pub stderr_tail: usize,
    pub empty_ok: bool,
}

impl GitPolicy {
    /// Read-oriented defaults: 8 MiB stdout, a 16 KiB stderr tail, and empty is
    /// a valid answer. These per-call defaults are not a global limit.
    pub const fn read(deadline: Duration) -> Self {
        Self {
            deadline,
            stdout_cap: 8 * 1024 * 1024,
            stderr_tail: 16 * 1024,
            empty_ok: true,
        }
    }
}

/// Result of one bounded Git command.
#[derive(Debug)]
pub enum GitOutcome {
    /// Successful command with non-empty stdout.
    Ok(Vec<u8>),
    /// Successful command with empty stdout, accepted by policy.
    EmptyOk,
    /// Successful command with empty stdout, refused by policy.
    EmptyRefused,
    /// Git exited unsuccessfully; stderr is the bounded tail.
    Exit {
        code: Option<i32>,
        stderr_tail: String,
    },
    Spawn(std::io::Error),
    Supervise(String),
    Timeout,
    Cancelled,
    OutputLimit(Which),
}

impl GitOutcome {
    pub fn is_success(&self) -> bool {
        matches!(self, Self::Ok(_) | Self::EmptyOk)
    }

    /// Decode successful stdout without replacing malformed UTF-8.
    pub fn into_text(self) -> Result<String, GitTextError> {
        match self {
            Self::Ok(bytes) => String::from_utf8(bytes).map_err(|_| GitTextError::InvalidUtf8),
            Self::EmptyOk => Ok(String::new()),
            other => Err(GitTextError::Failed(other)),
        }
    }
}

impl fmt::Display for GitOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ok(bytes) => write!(f, "Git succeeded with {} stdout bytes", bytes.len()),
            Self::EmptyOk => f.write_str("Git succeeded with empty stdout"),
            Self::EmptyRefused => {
                f.write_str("Git succeeded with empty stdout, which is not allowed")
            }
            Self::Exit { code, stderr_tail } => {
                write!(f, "Git exited with status {code:?}: {stderr_tail}")
            }
            Self::Spawn(error) => write!(f, "could not spawn Git: {error}"),
            Self::Supervise(reason) => write!(f, "Git supervision failed: {reason}"),
            Self::Timeout => f.write_str("Git command timed out"),
            Self::Cancelled => f.write_str("Git command was cancelled"),
            Self::OutputLimit(which) => write!(f, "Git exceeded the {which:?} output limit"),
        }
    }
}

/// Text conversion failure, preserving the typed command failure when present.
#[derive(Debug)]
pub enum GitTextError {
    InvalidUtf8,
    Failed(GitOutcome),
}

impl fmt::Display for GitTextError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidUtf8 => f.write_str("Git output is not valid UTF-8"),
            Self::Failed(outcome) => write!(f, "{outcome}"),
        }
    }
}

/// Execute an already-built command with bounded capture and process supervision.
pub fn run_command(cmd: &mut Command, policy: &GitPolicy, cancel: &AtomicBool) -> GitOutcome {
    let limits = Limits {
        quiesce_after_leader: Some(Duration::ZERO),
        term_grace: Duration::from_millis(200),
        reader_grace: Duration::from_millis(100),
        stdout: Stream::HeadOrFail {
            cap: policy.stdout_cap,
        },
        stderr: Stream::Tail {
            cap: policy.stderr_tail,
        },
        ..Limits::silent(policy.deadline)
    };
    let run = thegn_proc::run(cmd, &limits, cancel);
    outcome_from_run(run, policy)
}

fn outcome_from_run(run: thegn_proc::Run, policy: &GitPolicy) -> GitOutcome {
    match run.end {
        End::Spawn(error) => GitOutcome::Spawn(error),
        End::Supervise(error) => GitOutcome::Supervise(error.to_string()),
        End::TimedOut => GitOutcome::Timeout,
        End::Cancelled => GitOutcome::Cancelled,
        End::OutputLimit(which) => GitOutcome::OutputLimit(which),
        End::LeaderExited => match run.leader {
            Leader::Reaped(status) => {
                outcome_for_status(status, run.stdout.bytes, run.stderr.bytes, policy)
            }
            leader => {
                GitOutcome::Supervise(format!("leader was not synchronously reaped: {leader:?}"))
            }
        },
    }
}

fn outcome_for_status(
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    policy: &GitPolicy,
) -> GitOutcome {
    if status.success() {
        if stdout.is_empty() {
            if policy.empty_ok {
                GitOutcome::EmptyOk
            } else {
                GitOutcome::EmptyRefused
            }
        } else {
            GitOutcome::Ok(stdout)
        }
    } else {
        GitOutcome::Exit {
            code: status.code(),
            stderr_tail: bounded_stderr_tail(&stderr, policy.stderr_tail),
        }
    }
}

fn bounded_stderr_tail(bytes: &[u8], cap: usize) -> String {
    let mut tail = String::from_utf8_lossy(bytes).trim().to_string();
    if tail.len() > cap {
        let mut start = tail.len() - cap;
        while !tail.is_char_boundary(start) {
            start += 1;
        }
        tail.drain(..start);
    }
    tail
}

/// Run Git in `dir` using the core Git environment scrub and spawn counter.
/// This is blocking; call it off the UI loop.
pub fn git_run(dir: &Path, args: &[&str], policy: &GitPolicy, cancel: &AtomicBool) -> GitOutcome {
    crate::util::note_core_git_spawn();
    let mut cmd = crate::util::git_cmd(dir);
    cmd.args(args);
    run_command(&mut cmd, policy, cancel)
}

#[cfg(test)]
mod tests {
    use std::{
        process::Command,
        sync::atomic::AtomicBool,
        time::{Duration, Instant},
    };

    use super::{GitOutcome, GitPolicy, GitTextError, git_run, outcome_from_run, run_command};
    use crate::util;
    use thegn_proc::{Capture, Containment, End, GroupState, Leader, Run, Which};

    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let mut cmd = util::git_cmd(dir.path());
        cmd.args(["init", "-q"])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1");
        assert!(matches!(
            run_command(&mut cmd, &policy(), &AtomicBool::new(false)),
            GitOutcome::EmptyOk
        ));
        dir
    }

    fn policy() -> GitPolicy {
        GitPolicy::read(Duration::from_secs(10))
    }

    fn git_alias(dir: &std::path::Path, alias: &str, name: &str) -> Command {
        let mut cmd = util::git_cmd(dir);
        cmd.arg("-c")
            .arg(format!("alias.{name}={alias}"))
            .arg(name)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1");
        cmd
    }

    #[test]
    fn empty_success_is_distinct_from_failure() {
        let dir = repo();
        let mut cmd = util::git_cmd(dir.path());
        cmd.args(["status", "--porcelain"])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1");
        assert!(matches!(
            run_command(&mut cmd, &policy(), &AtomicBool::new(false)),
            GitOutcome::EmptyOk
        ));

        let strict = GitPolicy {
            empty_ok: false,
            ..policy()
        };
        let mut cmd = util::git_cmd(dir.path());
        cmd.args(["status", "--porcelain"])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1");
        assert!(matches!(
            run_command(&mut cmd, &strict, &AtomicBool::new(false)),
            GitOutcome::EmptyRefused
        ));
    }

    #[test]
    fn non_empty_success_keeps_bytes() {
        let dir = repo();
        let mut cmd = util::git_cmd(dir.path());
        cmd.args(["rev-parse", "--git-dir"])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1");
        match run_command(&mut cmd, &policy(), &AtomicBool::new(false)) {
            GitOutcome::Ok(bytes) => assert_eq!(String::from_utf8(bytes).unwrap().trim(), ".git"),
            other => panic!("expected non-empty Git success, got {other:?}"),
        }
    }

    #[test]
    fn non_zero_exit_keeps_the_stderr_tail() {
        let dir = repo();
        let mut cmd = git_alias(dir.path(), "!echo boom >&2; exit 3", "boom");
        assert!(
            matches!(run_command(&mut cmd, &policy(), &AtomicBool::new(false)), GitOutcome::Exit { code: Some(3), stderr_tail } if stderr_tail.contains("boom"))
        );
    }

    #[test]
    fn missing_program_is_a_spawn_failure() {
        let mut cmd = Command::new("thegn-no-such-binary-368a");
        assert!(
            matches!(run_command(&mut cmd, &policy(), &AtomicBool::new(false)), GitOutcome::Spawn(error) if error.kind() == std::io::ErrorKind::NotFound)
        );
    }

    #[test]
    fn invalid_utf8_is_reported_not_replaced() {
        let dir = repo();
        let mut cmd = git_alias(dir.path(), "!printf \\377\\376", "badutf8");
        let outcome = run_command(&mut cmd, &policy(), &AtomicBool::new(false));
        assert!(matches!(outcome, GitOutcome::Ok(_)));
        assert!(matches!(
            outcome.into_text(),
            Err(GitTextError::InvalidUtf8)
        ));
    }

    #[test]
    fn deadline_ends_a_hung_command() {
        let dir = repo();
        let mut cmd = git_alias(dir.path(), "!sleep 30", "hang");
        let started = Instant::now();
        let short = GitPolicy {
            deadline: Duration::from_millis(300),
            ..policy()
        };
        assert!(matches!(
            run_command(&mut cmd, &short, &AtomicBool::new(false)),
            GitOutcome::Timeout
        ));
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn cancel_flag_ends_the_run() {
        let dir = repo();
        let mut cmd = git_alias(dir.path(), "!sleep 30", "hang");
        let started = Instant::now();
        assert!(matches!(
            run_command(&mut cmd, &policy(), &AtomicBool::new(true)),
            GitOutcome::Cancelled
        ));
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn stdout_cap_ends_the_run_at_ingestion() {
        let dir = repo();
        let mut cmd = git_alias(dir.path(), "!yes x | head -c 200000", "stdoutbig");
        let limited = GitPolicy {
            stdout_cap: 1024,
            ..policy()
        };
        assert!(matches!(
            run_command(&mut cmd, &limited, &AtomicBool::new(false)),
            GitOutcome::OutputLimit(Which::Stdout)
        ));
    }

    #[test]
    fn stderr_tail_is_capped() {
        let dir = repo();
        let mut cmd = git_alias(
            dir.path(),
            "!yes e | head -c 100000 >&2; exit 1",
            "stderrbig",
        );
        let limited = GitPolicy {
            stderr_tail: 64,
            ..policy()
        };
        assert!(
            matches!(run_command(&mut cmd, &limited, &AtomicBool::new(false)), GitOutcome::Exit { stderr_tail, .. } if stderr_tail.len() <= 64)
        );
    }

    #[test]
    fn pipe_holding_descendant_does_not_block_the_read() {
        let dir = repo();
        let mut cmd = git_alias(dir.path(), "!sleep 30 & echo hi", "pipeholder");
        let started = Instant::now();
        match run_command(&mut cmd, &policy(), &AtomicBool::new(false)) {
            GitOutcome::Ok(bytes) => assert_eq!(String::from_utf8(bytes).unwrap().trim(), "hi"),
            other => panic!("expected success despite pipe holder, got {other:?}"),
        }
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn git_run_counts_the_spawn() {
        let dir = repo();
        let before = util::core_git_spawn_count();
        let outcome = git_run(
            dir.path(),
            &[
                "-c",
                "status.showUntrackedFiles=no",
                "status",
                "--porcelain",
            ],
            &policy(),
            &AtomicBool::new(false),
        );
        let after = util::core_git_spawn_count();
        assert!(matches!(outcome, GitOutcome::EmptyOk));
        assert!(after > before);
    }

    #[test]
    fn outcomes_report_success_and_preserve_failures_as_text_errors() {
        let success = GitOutcome::Ok(b"data".to_vec());
        assert!(success.is_success());
        assert_eq!(success.into_text().unwrap(), "data");
        assert!(GitOutcome::EmptyOk.is_success());
        assert!(!GitOutcome::EmptyRefused.is_success());
        assert_eq!(GitOutcome::EmptyOk.into_text().unwrap(), "");
        assert!(matches!(
            GitOutcome::Exit { code: Some(2), stderr_tail: "tail".to_owned() }.into_text(),
            Err(GitTextError::Failed(GitOutcome::Exit { stderr_tail, .. })) if stderr_tail == "tail"
        ));
        assert!(
            GitOutcome::Exit {
                code: Some(2),
                stderr_tail: "diagnostic".to_owned()
            }
            .to_string()
            .contains("diagnostic")
        );
        assert!(GitTextError::InvalidUtf8.to_string().contains("UTF-8"));
        assert!(
            GitTextError::Failed(GitOutcome::Exit {
                code: Some(9),
                stderr_tail: "visible diagnostic".to_owned(),
            })
            .to_string()
            .contains("visible diagnostic")
        );
        let display_cases = [
            GitOutcome::Ok(Vec::new()),
            GitOutcome::EmptyOk,
            GitOutcome::EmptyRefused,
            GitOutcome::Spawn(std::io::Error::other("spawn")),
            GitOutcome::Supervise("supervise".to_owned()),
            GitOutcome::Timeout,
            GitOutcome::Cancelled,
            GitOutcome::OutputLimit(Which::Stderr),
        ];
        assert!(
            display_cases
                .iter()
                .all(|outcome| !outcome.to_string().is_empty())
        );
    }

    #[test]
    fn invalid_stderr_remains_bounded_after_utf8_replacement() {
        let tail = super::bounded_stderr_tail(&[0xff; 10], 8);
        assert!(tail.len() <= 8);
        assert!(tail.ends_with('\u{fffd}'));
    }

    #[test]
    fn supervision_failures_are_typed_and_name_the_unreaped_leader() {
        let policy = policy();
        let supervise = Run {
            end: End::Supervise(std::io::Error::other("wait failed")),
            group: GroupState::Unknown,
            leader: Leader::NotSpawned,
            leader_exited_first: false,
            stdout: Capture::default(),
            stderr: Capture::default(),
            containment: Containment::ProcessGroup,
        };
        assert!(
            matches!(outcome_from_run(supervise, &policy), GitOutcome::Supervise(reason) if reason.contains("wait failed"))
        );

        let unreaped = Run {
            end: End::LeaderExited,
            group: GroupState::Unknown,
            leader: Leader::NotSpawned,
            leader_exited_first: false,
            stdout: Capture::default(),
            stderr: Capture::default(),
            containment: Containment::ProcessGroup,
        };
        assert!(
            matches!(outcome_from_run(unreaped, &policy), GitOutcome::Supervise(reason) if reason.contains("synchronously reaped"))
        );
    }
}
