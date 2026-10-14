//! Bounded process runner for Git commits created without an attached user.
//!
//! Commit messages still need stdin, so merely redirecting stdin to `/dev/null`
//! is not an option. This runner closes the message pipe before waiting and
//! removes every prompt surface. On Unix, a timeout kills the isolated process
//! group; other platforms kill and reap the direct child. It kills only the
//! commit's own process group. Signer descendants that escape it with `setsid`
//! or double-forking (for example, an autostarted `gpg-agent`) are intentionally
//! left alive: killing those user-owned agents would break them (THE-382 owner
//! decision, 2026-10-12). Windows process-tree containment is tracked by
//! THE-251. This runner is intentionally not used for general Git writes:
//! remote fetch/push operations must not inherit this local-commit deadline.

use anyhow::Result;
use std::io::{Read, Write};
use std::process::Stdio;
use std::time::{Duration, Instant};
use thegn_core::remote::GitLoc;

use super::GIT_SPAWNS;

const MAX_CAPTURE_BYTES: u64 = 1024 * 1024;
const MAX_STREAM_CAPTURE_BYTES: usize = (MAX_CAPTURE_BYTES / 2) as usize;
const MAX_BACKGROUND_COMMIT_MESSAGE_BYTES: usize = 1024 * 1024;
const MAX_BACKGROUND_COMMIT_TIMEOUT: Duration = Duration::from_secs(3600);
const CAPTURE_JOIN_GRACE: Duration = Duration::from_millis(500);

/// A background commit exceeded its dedicated non-interactive deadline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackgroundCommitTimeout {
    pub command: String,
    pub timeout: Duration,
}

impl std::fmt::Display for BackgroundCommitTimeout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "background git commit `{}` timed out after {:?} and was killed; a signing or hook subprocess may be waiting for input — configure non-interactive signing or disable signing for this background path",
            self.command, self.timeout
        )
    }
}

impl std::error::Error for BackgroundCommitTimeout {}

/// A typed failure from the bounded background commit runner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackgroundCommitFailure {
    MessageTooLarge {
        len: usize,
        limit: usize,
    },
    OutputTruncated {
        command: String,
        stream: &'static str,
        limit: usize,
    },
    LockUnavailable {
        lock_path: String,
        error: String,
    },
    StdinWriterPanicked,
    StdinWrite {
        error: String,
    },
    CaptureCleanup {
        stream: &'static str,
        detail: &'static str,
    },
    Spawn {
        command: String,
        error: String,
    },
    Wait {
        operation: &'static str,
        error: String,
    },
    Exited {
        command: String,
        detail: String,
        truncated: bool,
    },
}

impl std::fmt::Display for BackgroundCommitFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MessageTooLarge { len, limit } => write!(
                f,
                "background git commit message is {len} bytes, exceeding the {limit} byte limit"
            ),
            Self::OutputTruncated {
                command,
                stream,
                limit,
            } => write!(
                f,
                "git {command} {stream} exceeded its {limit} byte capture limit"
            ),
            Self::LockUnavailable { lock_path, error } => {
                write!(f, "git mutation lock `{lock_path}` is unavailable: {error}")
            }
            Self::StdinWriterPanicked => f.write_str("background git stdin writer panicked"),
            Self::StdinWrite { error } => {
                write!(f, "write background git commit message: {error}")
            }
            Self::CaptureCleanup { stream, detail } => {
                write!(f, "background git {stream} reader {detail}")
            }
            Self::Spawn { command, error } => write!(f, "git {command}: {error}"),
            Self::Wait { operation, error } => write!(f, "{operation}: {error}"),
            Self::Exited {
                command,
                detail,
                truncated,
            } => {
                write!(f, "git {command} failed: {}", bounded_error_detail(detail))?;
                if *truncated {
                    f.write_str("\n[output truncated]")?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for BackgroundCommitFailure {}

const BACKGROUND_COMMIT_TIMEOUT_ENV: &str = "THEGN_BACKGROUND_COMMIT_TIMEOUT_SECS";

fn parse_background_commit_timeout(value: Option<&str>) -> Duration {
    let seconds = value
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|seconds| *seconds > 0)
        .unwrap_or(120);
    Duration::from_secs(seconds).min(MAX_BACKGROUND_COMMIT_TIMEOUT)
}

fn background_commit_timeout() -> Duration {
    parse_background_commit_timeout(std::env::var(BACKGROUND_COMMIT_TIMEOUT_ENV).ok().as_deref())
}

/// Run an automated `commit`/`commit-tree` with its message on stdin.
///
/// The message pipe is written and then closed; no terminal, graphical
/// askpass, or inherited GPG tty is available afterwards. On timeout the whole
/// Unix process group is killed and the direct child is always reaped.
pub(crate) fn run(
    loc: &GitLoc,
    envs: &[(&str, &str)],
    args: &[&str],
    stdin: &[u8],
) -> Result<String> {
    run_with_timeout(loc, envs, args, stdin, background_commit_timeout())
}

pub(crate) fn run_with_timeout(
    loc: &GitLoc,
    envs: &[(&str, &str)],
    args: &[&str],
    stdin: &[u8],
    timeout: Duration,
) -> Result<String> {
    if stdin.len() > MAX_BACKGROUND_COMMIT_MESSAGE_BYTES {
        return Err(BackgroundCommitFailure::MessageTooLarge {
            len: stdin.len(),
            limit: MAX_BACKGROUND_COMMIT_MESSAGE_BYTES,
        }
        .into());
    }
    let timeout = timeout.min(MAX_BACKGROUND_COMMIT_TIMEOUT);
    let _lock = if loc.is_remote() {
        None
    } else {
        let worktree = std::path::Path::new(&loc.path()).to_path_buf();
        let lock_path = thegn_core::util::git_common_dir(&worktree).join("thegn-git.lock");
        Some(
            thegn_core::util::try_lock_git_mutations_within(&worktree, timeout).map_err(
                |error| BackgroundCommitFailure::LockUnavailable {
                    lock_path: lock_path.display().to_string(),
                    error: error.to_string(),
                },
            )?,
        )
    };
    let mut env: Vec<(&str, &str)> = vec![
        ("GIT_TERMINAL_PROMPT", "0"),
        ("GIT_ASKPASS", ""),
        ("SSH_ASKPASS_REQUIRE", "never"),
        ("GPG_TTY", ""),
        ("DISPLAY", ""),
        ("WAYLAND_DISPLAY", ""),
    ];
    env.extend_from_slice(envs);
    // Compute before spawning anything. `checked_add` avoids overflowing even
    // if this function is called with `Duration::MAX`.
    let now = Instant::now();
    let deadline = now
        .checked_add(timeout)
        .or_else(|| now.checked_add(MAX_BACKGROUND_COMMIT_TIMEOUT))
        .unwrap_or(now);
    let mut cmd = loc.git_command_env(&env, args);
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    isolate_process(&mut cmd);
    GIT_SPAWNS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut child = cmd
        .spawn()
        .map_err(|error| BackgroundCommitFailure::Spawn {
            command: args.join(" "),
            error: error.to_string(),
        })?;
    let pid = child.id();
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let stdout_rx = spawn_capture_reader(stdout, "stdout");
    let stderr_rx = spawn_capture_reader(stderr, "stderr");
    let mut input = child.stdin.take().expect("piped stdin");
    let bytes = stdin.to_vec();
    let (input_tx, input_rx) = std::sync::mpsc::sync_channel(1);
    let input_h = std::thread::spawn(move || {
        let result = input.write_all(&bytes);
        drop(input); // EOF tells `-F -` that the commit message is complete.
        // A closed receiver means the deadline path already owns cleanup.
        if input_tx.send(result).is_err() {}
    });
    // Finish and close the sole stdin pipe before waiting for the commit. Git
    // cannot accidentally hand the commit-message stream to a signing prompt.
    let input_result =
        match input_rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(result) => result,
            Err(_) => {
                kill_process_tree(pid);
                kill_and_reap(&mut child);
                join_worker(input_h, "stdin writer");
                collect_captures_best_effort(stdout_rx, stderr_rx);
                return Err(timeout_error(args, timeout));
            }
        };
    if input_h.join().is_err() {
        kill_process_tree(pid);
        kill_and_reap(&mut child);
        collect_captures_best_effort(stdout_rx, stderr_rx);
        return Err(BackgroundCommitFailure::StdinWriterPanicked.into());
    }
    let mut backoff = Duration::from_millis(1);
    loop {
        let exited = match crate::plugin::proc::leader_exited_nowait(pid) {
            Ok(exited) => exited,
            Err(error) if error.kind() == std::io::ErrorKind::Unsupported => match child.try_wait()
            {
                Ok(Some(_)) => true,
                Ok(None) => false,
                Err(error) => {
                    kill_process_tree(pid);
                    kill_and_reap(&mut child);
                    collect_captures_best_effort(stdout_rx, stderr_rx);
                    return Err(BackgroundCommitFailure::Wait {
                        operation: "wait for background git commit",
                        error: error.to_string(),
                    }
                    .into());
                }
            },
            Err(error) => {
                kill_process_tree(pid);
                kill_and_reap(&mut child);
                collect_captures_best_effort(stdout_rx, stderr_rx);
                return Err(BackgroundCommitFailure::Wait {
                    operation: "observe background git commit exit",
                    error: error.to_string(),
                }
                .into());
            }
        };
        if exited {
            break;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            kill_process_tree(pid);
            kill_and_reap(&mut child);
            collect_captures_best_effort(stdout_rx, stderr_rx);
            return Err(timeout_error(args, timeout));
        }
        std::thread::sleep(backoff.min(remaining));
        backoff = (backoff * 2).min(Duration::from_millis(25));
    }

    // The zombie leader pins its pid/pgid. Kill descendants before reaping it,
    // even after a successful commit, so they cannot retain the pipe handles.
    kill_process_tree(pid);
    let status = child.wait();
    let capture_deadline = Instant::now()
        .checked_add(CAPTURE_JOIN_GRACE)
        .unwrap_or_else(Instant::now);
    let stdout_result = collect_capture(stdout_rx, "stdout", capture_deadline);
    let stderr_result = collect_capture(stderr_rx, "stderr", capture_deadline);
    let status = status.map_err(|error| BackgroundCommitFailure::Wait {
        operation: "reap background git commit",
        error: error.to_string(),
    })?;
    let (stdout, stdout_truncated) = stdout_result?;
    let (stderr, stderr_truncated) = stderr_result?;
    if stdout_truncated && status.success() {
        return Err(BackgroundCommitFailure::OutputTruncated {
            command: args.join(" "),
            stream: "stdout",
            limit: MAX_STREAM_CAPTURE_BYTES,
        }
        .into());
    }
    if !status.success() {
        let stderr = String::from_utf8_lossy(&stderr);
        let stdout_text = String::from_utf8_lossy(&stdout);
        let detail = match (stderr.trim(), stdout_text.trim()) {
            ("", "") => String::new(),
            ("", output) => output.to_string(),
            (error, "") => error.to_string(),
            (error, output) => format!("{error}\n{output}"),
        };
        let truncated = stdout_truncated || stderr_truncated;
        return Err(BackgroundCommitFailure::Exited {
            command: args.join(" "),
            detail,
            truncated,
        }
        .into());
    }
    if stderr_truncated {
        return Err(BackgroundCommitFailure::OutputTruncated {
            command: args.join(" "),
            stream: "stderr",
            limit: MAX_STREAM_CAPTURE_BYTES,
        }
        .into());
    }
    input_result.map_err(|error| BackgroundCommitFailure::StdinWrite {
        error: error.to_string(),
    })?;
    Ok(String::from_utf8_lossy(&stdout).into_owned())
}

fn timeout_error(args: &[&str], timeout: Duration) -> anyhow::Error {
    BackgroundCommitTimeout {
        command: args.join(" "),
        timeout,
    }
    .into()
}

fn read_capped(mut reader: impl Read, limit: usize) -> (Vec<u8>, bool) {
    let mut output = Vec::with_capacity(limit.min(8192));
    let mut truncated = false;
    let mut buffer = [0u8; 8192];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(count) => {
                let room = limit.saturating_sub(output.len());
                let keep = count.min(room);
                output.extend_from_slice(&buffer[..keep]);
                truncated |= keep < count;
            }
        }
    }
    (output, truncated)
}

fn spawn_capture_reader(
    reader: impl Read + Send + 'static,
    name: &'static str,
) -> std::sync::mpsc::Receiver<(Vec<u8>, bool)> {
    let (sender, receiver) = std::sync::mpsc::channel();
    drop(std::thread::spawn(move || {
        let capture = read_capped(reader, MAX_STREAM_CAPTURE_BYTES);
        if let Err(error) = sender.send(capture) {
            tracing::debug!(target: "thegn::git", worker = name, %error, "background git output receiver was dropped");
        }
    }));
    receiver
}

fn collect_capture(
    receiver: std::sync::mpsc::Receiver<(Vec<u8>, bool)>,
    name: &str,
    deadline: Instant,
) -> Result<(Vec<u8>, bool)> {
    match receiver.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        Ok(capture) => Ok(capture),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            Err(BackgroundCommitFailure::CaptureCleanup {
                stream: if name == "stdout" { "stdout" } else { "stderr" },
                detail: "exceeded its cleanup grace",
            }
            .into())
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            Err(BackgroundCommitFailure::CaptureCleanup {
                stream: if name == "stdout" { "stdout" } else { "stderr" },
                detail: "stopped without a capture",
            }
            .into())
        }
    }
}

fn join_worker<T>(handle: std::thread::JoinHandle<T>, name: &str) {
    if handle.join().is_err() {
        tracing::debug!(target: "thegn::git", worker = name, "background git worker panicked during cleanup");
    }
}

fn collect_captures_best_effort(
    stdout_rx: std::sync::mpsc::Receiver<(Vec<u8>, bool)>,
    stderr_rx: std::sync::mpsc::Receiver<(Vec<u8>, bool)>,
) {
    let deadline = Instant::now()
        .checked_add(CAPTURE_JOIN_GRACE)
        .unwrap_or_else(Instant::now);
    for (receiver, name) in [(stdout_rx, "stdout"), (stderr_rx, "stderr")] {
        if let Err(error) = collect_capture(receiver, name, deadline) {
            tracing::debug!(target: "thegn::git", %error, "background git output cleanup failed");
        }
    }
}

fn bounded_error_detail(detail: &str) -> &str {
    const MAX_ERROR_DETAIL_BYTES: usize = 1024 * 1024 - 512;
    let mut end = detail.len().min(MAX_ERROR_DETAIL_BYTES);
    while !detail.is_char_boundary(end) {
        end -= 1;
    }
    &detail[..end]
}

fn kill_and_reap(child: &mut std::process::Child) {
    // Cleanup is best effort because callers must retain the primary timeout
    // or wait error. `wait` is still attempted when `kill` reports that the
    // child has already exited so no zombie is left behind.
    if let Err(error) = child.kill() {
        tracing::debug!(target: "thegn::git", %error, "could not kill background git child during cleanup");
    }
    if let Err(error) = child.wait() {
        tracing::debug!(target: "thegn::git", %error, "could not reap background git child during cleanup");
    }
}

#[cfg(unix)]
fn isolate_process(cmd: &mut std::process::Command) {
    use std::os::unix::process::CommandExt;
    // A new session removes access to the compositor's controlling terminal;
    // its process-group id is the child pid, which also gives timeout cleanup a
    // precise descendant target.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
}

#[cfg(not(unix))]
fn isolate_process(_cmd: &mut std::process::Command) {}

#[cfg(unix)]
fn kill_process_tree(pid: u32) {
    crate::plugin::proc::kill_group(pid);
}

#[cfg(not(unix))]
fn kill_process_tree(_pid: u32) {}

#[cfg(test)]
mod public_api_tests {
    #[test]
    fn background_commit_failure_is_reexported() {
        let failure = crate::git::BackgroundCommitFailure::MessageTooLarge { len: 2, limit: 1 };
        assert!(matches!(
            failure,
            crate::git::BackgroundCommitFailure::MessageTooLarge { len: 2, limit: 1 }
        ));
    }
}

#[cfg(all(test, unix))]
mod unix_tests {
    use super::{
        BackgroundCommitFailure, BackgroundCommitTimeout, MAX_BACKGROUND_COMMIT_MESSAGE_BYTES,
        MAX_BACKGROUND_COMMIT_TIMEOUT, collect_capture, parse_background_commit_timeout,
        run_with_timeout,
    };
    use crate::git::testutil::{TestRepo, git_in};
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    fn ident(dir: &Path) {
        git_in(dir, &["config", "user.name", "t"]);
        git_in(dir, &["config", "user.email", "t@e"]);
        git_in(dir, &["config", "commit.gpgsign", "false"]);
    }

    #[test]
    fn io_failures_include_the_underlying_cause_in_display() {
        let failures = [
            (
                BackgroundCommitFailure::Spawn {
                    command: "commit-tree".to_string(),
                    error: "No such file or directory (os error 2)".to_string(),
                },
                "git commit-tree: No such file or directory (os error 2)",
            ),
            (
                BackgroundCommitFailure::Wait {
                    operation: "wait for background git commit",
                    error: "wait failed".to_string(),
                },
                "wait for background git commit: wait failed",
            ),
            (
                BackgroundCommitFailure::StdinWrite {
                    error: "Broken pipe (os error 32)".to_string(),
                },
                "write background git commit message: Broken pipe (os error 32)",
            ),
        ];

        for (failure, expected) in failures {
            assert_eq!(failure.to_string(), expected);
        }
    }

    #[test]
    fn timeout_parser_clamps_huge_values_and_defaults_invalid_values() {
        assert_eq!(
            parse_background_commit_timeout(Some("18446744073709551615")),
            MAX_BACKGROUND_COMMIT_TIMEOUT
        );
        assert_eq!(
            parse_background_commit_timeout(Some("0")),
            std::time::Duration::from_secs(120)
        );
        assert_eq!(
            parse_background_commit_timeout(Some("nope")),
            std::time::Duration::from_secs(120)
        );
    }

    #[test]
    fn capture_collection_returns_when_a_writer_keeps_the_pipe_open() {
        let (sender, receiver) = std::sync::mpsc::channel();
        let started = std::time::Instant::now();
        let deadline = started + std::time::Duration::from_millis(25);
        let error = collect_capture(receiver, "stdout", deadline)
            .expect_err("an open pipe must not block capture collection indefinitely");

        assert!(
            started.elapsed() < std::time::Duration::from_millis(250),
            "capture collection exceeded its deadline: {error:#}"
        );
        assert!(error.to_string().contains("cleanup grace"), "{error:#}");
        drop(sender);
    }

    #[test]
    fn maximum_duration_does_not_panic_or_abandon_commit() {
        let repo = TestRepo::new("plumb-max-timeout");
        ident(&repo.dir);
        repo.commit_file("f.txt", "one\n", "c0");
        let loc = repo.loc();
        let head = repo.head();
        let tree = repo.out(&["rev-parse", "HEAD^{tree}"]);
        let sha = run_with_timeout(
            &loc,
            &[],
            &["commit-tree", &tree, "-p", &head],
            b"bounded timeout\n",
            std::time::Duration::MAX,
        )
        .expect("a large configured timeout is clamped before spawn");
        assert_eq!(repo.out(&["cat-file", "-t", sha.trim()]), "commit");
    }

    #[test]
    fn oversized_message_is_refused_before_commit_creation() {
        let repo = TestRepo::new("plumb-oversized-message");
        ident(&repo.dir);
        repo.commit_file("f.txt", "one\n", "c0");
        let loc = repo.loc();
        let head = repo.head();
        let tree = repo.out(&["rev-parse", "HEAD^{tree}"]);
        let message = vec![b'x'; MAX_BACKGROUND_COMMIT_MESSAGE_BYTES + 1];

        let error = run_with_timeout(
            &loc,
            &[],
            &["commit-tree", &tree, "-p", &head],
            &message,
            std::time::Duration::from_secs(2),
        )
        .expect_err("oversized commit messages must be rejected before spawning git");

        assert!(
            matches!(
                error.downcast_ref::<BackgroundCommitFailure>(),
                Some(BackgroundCommitFailure::MessageTooLarge { len, limit })
                    if *len == MAX_BACKGROUND_COMMIT_MESSAGE_BYTES + 1
                        && *limit == MAX_BACKGROUND_COMMIT_MESSAGE_BYTES
            ),
            "message limit must be a typed failure: {error:#}"
        );
        assert_eq!(repo.head(), head);
    }

    #[test]
    fn lock_failure_fails_closed_without_committing() {
        let repo = TestRepo::new("plumb-lock-failure");
        ident(&repo.dir);
        repo.commit_file("f.txt", "one\n", "c0");
        let loc = repo.loc();
        let head = repo.head();
        std::fs::create_dir(repo.dir.join(".git/thegn-git.lock")).unwrap();
        std::fs::write(repo.dir.join("staged.txt"), "staged\n").unwrap();
        git_in(&repo.dir, &["add", "staged.txt"]);

        let error = run_with_timeout(
            &loc,
            &[],
            &["commit", "-F", "-"],
            b"must remain staged\n",
            std::time::Duration::from_secs(2),
        )
        .expect_err("a required local mutation lock failure must stop the commit");

        assert!(
            matches!(
                error.downcast_ref::<BackgroundCommitFailure>(),
                Some(BackgroundCommitFailure::LockUnavailable { .. })
            ),
            "lock failure must be typed: {error:#}"
        );
        assert_eq!(repo.head(), head);
        assert_eq!(repo.out(&["diff", "--cached", "--name-only"]), "staged.txt");
    }

    #[test]
    fn held_lock_bounds_background_commit_by_its_timeout() {
        let repo = TestRepo::new("plumb-lock-timeout");
        ident(&repo.dir);
        repo.commit_file("f.txt", "one\n", "c0");
        let loc = repo.loc();
        let head = repo.head();
        std::fs::write(repo.dir.join("staged.txt"), "staged\n").unwrap();
        git_in(&repo.dir, &["add", "staged.txt"]);
        let held = thegn_core::util::try_lock_git_mutations(&repo.dir).unwrap();
        let worker_loc = loc.clone();
        let (sender, receiver) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            sender
                .send(run_with_timeout(
                    &worker_loc,
                    &[],
                    &["commit", "-F", "-"],
                    b"must remain staged\n",
                    std::time::Duration::from_millis(200),
                ))
                .unwrap();
        });

        let received = receiver.recv_timeout(std::time::Duration::from_secs(10));
        drop(held);
        worker.join().unwrap();
        let error = received
            .expect("background commit must return while the lock remains held")
            .unwrap_err();
        assert!(
            matches!(
                error.downcast_ref::<BackgroundCommitFailure>(),
                Some(BackgroundCommitFailure::LockUnavailable { .. })
            ),
            "lock wait must be a typed failure: {error:#}"
        );
        assert!(error.to_string().contains("timed out"), "{error}");
        assert_eq!(repo.head(), head);
        assert_eq!(repo.out(&["diff", "--cached", "--name-only"]), "staged.txt");
    }

    #[test]
    fn timeout_kills_signer_tree_reaps_git_and_preserves_ref() {
        let repo = TestRepo::new("plumb-sign-timeout");
        ident(&repo.dir);
        repo.commit_file("f.txt", "one\n", "c0");
        let loc = repo.loc();
        let head = repo.head();
        let tree = repo.out(&["rev-parse", "HEAD^{tree}"]);
        let pid_file = repo.dir.join("hung-signer.pid");
        let signer = repo.dir.join("hung-signer.sh");
        std::fs::write(
            &signer,
            format!(
                "#!/bin/sh\nsleep 30 &\nprintf '%s' \"$!\" > '{}'\nwait\n",
                pid_file.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&signer, std::fs::Permissions::from_mode(0o755)).unwrap();
        git_in(
            &repo.dir,
            &["config", "gpg.program", signer.to_str().unwrap()],
        );

        let error = run_with_timeout(
            &loc,
            &[],
            &["commit-tree", &tree, "-p", &head, "-S"],
            b"signed fold\n",
            std::time::Duration::from_millis(500),
        )
        .expect_err("the hanging signer must hit the background deadline");
        assert!(
            error.downcast_ref::<BackgroundCommitTimeout>().is_some(),
            "timeout remains actionable and typed: {error:#}"
        );
        assert!(error.to_string().contains("500ms"), "{error:#}");
        assert!(error.to_string().contains("signing or hook"), "{error:#}");
        assert_eq!(repo.head(), head, "commit-tree failure must not move HEAD");

        let signer_pid: i32 = std::fs::read_to_string(&pid_file)
            .expect("hanging signer recorded its child")
            .parse()
            .unwrap();
        let gone = (0..100).any(|_| {
            let dead = unsafe { libc::kill(signer_pid, 0) } == -1
                && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
            if !dead {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            dead
        });
        assert!(gone, "timed-out signer descendant {signer_pid} survived");
    }

    #[test]
    fn kills_output_handle_descendant_after_hook_failure() {
        // A signer has a separate Git status fd, so retaining that fd correctly
        // keeps Git itself alive and exercises the ordinary deadline. A hook
        // isolates the post-Git-exit case: its child retains stdout/stderr only.
        let repo = TestRepo::new("plumb-hook-output-child");
        ident(&repo.dir);
        repo.commit_file("f.txt", "one\n", "c0");
        let loc = repo.loc();
        let head = repo.head();
        let pid_file = repo.dir.join("output-holder.pid");
        let hook = repo.dir.join(".git/hooks/pre-commit");
        std::fs::write(
            &hook,
            format!(
                "#!/bin/sh\nsleep 30 &\nprintf '%s' \"$!\" > '{}'\nexit 1\n",
                pid_file.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(repo.dir.join("staged.txt"), "staged\n").unwrap();
        git_in(&repo.dir, &["add", "staged.txt"]);

        let started = std::time::Instant::now();
        let deadline = std::time::Duration::from_secs(2);
        let error = run_with_timeout(
            &loc,
            &[],
            &["commit", "-F", "-"],
            b"background commit\n",
            deadline,
        )
        .expect_err("the hook exits unsuccessfully");
        assert!(
            started.elapsed() < deadline,
            "inherited output handles held the caller open for {:?}: {error:#}",
            started.elapsed()
        );
        assert!(error.to_string().contains("failed"), "{error:#}");
        assert_eq!(repo.head(), head);

        let holder_pid: i32 = std::fs::read_to_string(&pid_file)
            .expect("hook recorded the output-holder child")
            .parse()
            .unwrap();
        let gone = (0..100).any(|_| {
            let dead = unsafe { libc::kill(holder_pid, 0) } == -1
                && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
            if !dead {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            dead
        });
        assert!(
            gone,
            "output-holder child {holder_pid} survived background commit cleanup"
        );
    }

    #[test]
    fn successful_signer_descendants_are_killed_before_return() {
        let repo = TestRepo::new("plumb-signer-output-child");
        ident(&repo.dir);
        repo.commit_file("f.txt", "one\n", "c0");
        let loc = repo.loc();
        let head = repo.head();
        let tree = repo.out(&["rev-parse", "HEAD^{tree}"]);
        let pid_file = repo.dir.join("signer-child.pid");
        let signer = repo.dir.join("successful-signer.sh");
        std::fs::write(
            &signer,
            format!(
                "#!/bin/sh\nsleep 30 </dev/null >/dev/null 2>&1 &\nprintf '%s' \"$!\" > '{}'\ncat >/dev/null\nprintf '\\n[GNUPG:] SIG_CREATED D 1 8 00 0 FAKE\\n' >&2\nprintf '%s\\n' '-----BEGIN PGP SIGNATURE-----' 'fake' '-----END PGP SIGNATURE-----'\n",
                pid_file.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&signer, std::fs::Permissions::from_mode(0o755)).unwrap();
        git_in(
            &repo.dir,
            &["config", "gpg.program", signer.to_str().unwrap()],
        );

        run_with_timeout(
            &loc,
            &[],
            &["commit-tree", &tree, "-p", &head, "-S"],
            b"signed fold\n",
            std::time::Duration::from_secs(2),
        )
        .expect("the test signer returns a signature");
        let signer_pid: i32 = std::fs::read_to_string(&pid_file)
            .expect("signer recorded its child")
            .parse()
            .unwrap();
        let gone = (0..100).any(|_| {
            let dead = unsafe { libc::kill(signer_pid, 0) } == -1
                && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
            if !dead {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            dead
        });
        assert!(
            gone,
            "successful signer descendant {signer_pid} survived return"
        );
    }

    #[test]
    fn oversized_hook_stderr_is_drained_and_capped() {
        let repo = TestRepo::new("plumb-hook-output-cap");
        ident(&repo.dir);
        repo.commit_file("f.txt", "one\n", "c0");
        let loc = repo.loc();
        let hook = repo.dir.join(".git/hooks/pre-commit");
        std::fs::write(&hook, "#!/bin/sh\nhead -c 8000000 /dev/zero >&2\nexit 1\n").unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(repo.dir.join("staged.txt"), "staged\n").unwrap();
        git_in(&repo.dir, &["add", "staged.txt"]);

        let error = run_with_timeout(
            &loc,
            &[],
            &["commit", "-F", "-"],
            b"bounded output\n",
            std::time::Duration::from_secs(5),
        )
        .expect_err("the hook fails after writing oversized stderr");
        assert!(
            matches!(
                error.downcast_ref::<BackgroundCommitFailure>(),
                Some(BackgroundCommitFailure::Exited {
                    truncated: true,
                    ..
                })
            ),
            "the nonzero hook exit and its truncation must be typed: {error:#}"
        );
        assert!(
            error.to_string().len() <= 1024 * 1024,
            "error was {} bytes",
            error.to_string().len()
        );
        assert!(error.to_string().contains("output truncated"), "{error:#}");
    }
}
