//! User-defined git commands (`[[git_commands]]`): the host collects prompt
//! responses, thegn-core's `custom_cmd::compile` admits the template
//! against the current selection, and this runs the result in the worktree.
//! `terminal` output mode is executed by the host's floating-pane machinery
//! instead — this seam only covers capture (`popup`) and fire-and-forget.

use super::GitBackend;
use std::io::Read;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use thegn_core::remote::GitLoc;

pub const CUSTOM_COMMAND_TIMEOUT_SECS: u64 = thegn_core::custom_cmd::DEFAULT_TIMEOUT_SECS;
const CUSTOM_COMMAND_TIMEOUT: Duration = Duration::from_secs(CUSTOM_COMMAND_TIMEOUT_SECS);
const STDOUT_LIMIT: usize = 1024 * 1024;
const STDERR_LIMIT: usize = 64 * 1024;
const POLL_INTERVAL: Duration = Duration::from_millis(20);
const READER_GRACE: Duration = Duration::from_secs(2);
pub const CUSTOM_OUTCOME_EXITED: &str = "exited";
pub const CUSTOM_OUTCOME_TIMEOUT: &str = "timeout";
pub const CUSTOM_OUTCOME_SPAWN: &str = "spawn";
pub const CUSTOM_OUTCOME_WAIT: &str = "wait";
pub const CUSTOM_OUTCOME_READ: &str = "read";
pub const CUSTOM_OUTCOME_CLEANUP: &str = "cleanup";

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
    ) -> (&'static str, Option<ExitStatus>, String, bool, String) {
        let mut command = loc.sh_command(command);
        command.stdin(Stdio::null()).env("GIT_TERMINAL_PROMPT", "0");
        run_bounded(
            command,
            capture,
            CUSTOM_COMMAND_TIMEOUT,
            STDOUT_LIMIT,
            STDERR_LIMIT,
        )
        .into_parts()
    }

    /// Execute only a command admitted by the custom-template compiler.
    fn run_custom_template(
        &self,
        loc: &GitLoc,
        command: &thegn_core::custom_cmd::ExpandedCommand,
        capture: bool,
    ) -> (&'static str, Option<ExitStatus>, String, bool, String) {
        let timeout = command.timeout();
        let mut command = command.command(loc);
        command.stdin(Stdio::null()).env("GIT_TERMINAL_PROMPT", "0");
        run_bounded(command, capture, timeout, STDOUT_LIMIT, STDERR_LIMIT).into_parts()
    }
}

impl<T: GitBackend + ?Sized> CustomOps for T {}

fn run_bounded(
    command: Command,
    capture: bool,
    timeout: Duration,
    stdout_limit: usize,
    stderr_limit: usize,
) -> CustomOutcome {
    let mut command = command;
    if capture {
        command.stdout(Stdio::piped());
    } else {
        command.stdout(Stdio::null());
    }
    command.stderr(Stdio::piped());
    crate::plugin::proc::set_process_group(&mut command);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(_) => return CustomOutcome::Spawn,
    };
    let pid = child.id();
    let (stdout_rx, stdout_reader) = match child.stdout.take() {
        Some(stdout) if capture => {
            let (tx, rx) = mpsc::sync_channel(1);
            let reader = match std::thread::Builder::new().spawn(move || {
                let result = read_bounded(stdout, stdout_limit, false);
                if tx.send(result).is_err() {
                    tracing::debug!("custom command stdout reader detached");
                }
            }) {
                Ok(reader) => reader,
                Err(_) => return reap_after_spawn_failure(&mut child, pid, None),
            };
            (Some(rx), Some(reader))
        }
        _ => (None, None),
    };
    let (stderr_rx, stderr_reader) = match child.stderr.take() {
        Some(stderr) => {
            let (tx, rx) = mpsc::sync_channel(1);
            let reader = match std::thread::Builder::new().spawn(move || {
                let result = read_bounded(stderr, stderr_limit, true);
                if tx.send(result).is_err() {
                    tracing::debug!("custom command stderr reader detached");
                }
            }) {
                Ok(reader) => reader,
                Err(_) => {
                    return reap_after_spawn_failure(&mut child, pid, stdout_reader);
                }
            };
            (rx, Some(reader))
        }
        None => {
            crate::plugin::proc::kill_group(pid);
            if child.wait().is_err() {
                return CustomOutcome::Wait;
            }
            if let Some(reader) = stdout_reader
                && reader.join().is_err()
            {
                return CustomOutcome::IncompleteCleanup;
            }
            return CustomOutcome::Read;
        }
    };

    let deadline = Instant::now() + timeout;
    let mut timed_out = false;
    let status = loop {
        match crate::plugin::proc::leader_exited_nowait(pid) {
            Ok(true) => {
                // Keep the leader unreaped until its process group is gone:
                // descendants may still hold either output pipe open.
                crate::plugin::proc::kill_group(pid);
                break child.wait();
            }
            Ok(false) => {}
            Err(error) if error.kind() == std::io::ErrorKind::Unsupported => {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        crate::plugin::proc::kill_group(pid);
                        break Ok(status);
                    }
                    Ok(None) => {}
                    Err(error) => break Err(error),
                }
            }
            Err(_) => {
                crate::plugin::proc::kill_group(pid);
                if let Err(error) = child.wait() {
                    tracing::debug!(%error, "custom command reap failed");
                }
                return CustomOutcome::Wait;
            }
        }
        if Instant::now() >= deadline {
            timed_out = true;
            crate::plugin::proc::kill_group(pid);
            break child.wait();
        }
        std::thread::sleep(POLL_INTERVAL);
    };
    let status = match status {
        Ok(status) => status,
        Err(_) => {
            crate::plugin::proc::kill_group(pid);
            if let Err(error) = child.wait() {
                tracing::debug!(%error, "custom command reap failed");
            }
            return CustomOutcome::Wait;
        }
    };

    let reader_deadline = Instant::now() + READER_GRACE;
    let stdout = match stdout_rx {
        Some(receiver) => match recv_reader(receiver, reader_deadline) {
            Ok(result) => result,
            Err(outcome) => return outcome,
        },
        None => Ok((Vec::new(), false)),
    };
    let stderr = match recv_reader(stderr_rx, reader_deadline) {
        Ok(result) => result,
        Err(outcome) => return outcome,
    };
    if let Some(reader) = stdout_reader
        && reader.join().is_err()
    {
        return CustomOutcome::IncompleteCleanup;
    }
    if let Some(reader) = stderr_reader
        && reader.join().is_err()
    {
        return CustomOutcome::IncompleteCleanup;
    }
    if timed_out {
        return CustomOutcome::Timeout;
    }
    let (stdout, stdout_truncated) = match stdout {
        Ok(stdout) => stdout,
        Err(_) => return CustomOutcome::Read,
    };
    let (stderr, _) = match stderr {
        Ok(stderr) => stderr,
        Err(_) => return CustomOutcome::Read,
    };
    let (stdout, decode_truncated) = bounded_lossy_utf8(&stdout, stdout_limit);
    let (stderr, _) = bounded_lossy_utf8(&stderr, stderr_limit);
    CustomOutcome::Exited {
        status,
        stdout,
        stdout_truncated: stdout_truncated || decode_truncated,
        stderr,
    }
}

fn reap_after_spawn_failure(
    child: &mut Child,
    pid: u32,
    stdout_reader: Option<std::thread::JoinHandle<()>>,
) -> CustomOutcome {
    crate::plugin::proc::kill_group(pid);
    if child.wait().is_err() {
        return CustomOutcome::Wait;
    }
    if let Some(reader) = stdout_reader
        && reader.join().is_err()
    {
        return CustomOutcome::IncompleteCleanup;
    }
    CustomOutcome::Spawn
}

fn recv_reader(
    receiver: mpsc::Receiver<std::io::Result<(Vec<u8>, bool)>>,
    deadline: Instant,
) -> Result<std::io::Result<(Vec<u8>, bool)>, CustomOutcome> {
    receiver
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .map_err(|_| CustomOutcome::IncompleteCleanup)
}

fn read_bounded(
    mut reader: impl Read,
    limit: usize,
    keep_tail: bool,
) -> std::io::Result<(Vec<u8>, bool)> {
    let mut retained = Vec::with_capacity(limit.min(8192));
    let mut truncated = false;
    let mut chunk = [0u8; 8192];
    loop {
        let count = reader.read(&mut chunk)?;
        if count == 0 {
            return Ok((retained, truncated));
        }
        if keep_tail {
            if count >= limit {
                truncated |= retained.len() + count > limit;
                retained.clear();
                retained.extend_from_slice(&chunk[count - limit..count]);
            } else {
                let overflow = retained.len().saturating_add(count).saturating_sub(limit);
                if overflow > 0 {
                    retained.drain(..overflow);
                    truncated = true;
                }
                retained.extend_from_slice(&chunk[..count]);
            }
        } else if retained.len() < limit {
            let keep = (limit - retained.len()).min(count);
            retained.extend_from_slice(&chunk[..keep]);
            truncated |= keep < count;
        } else {
            truncated = true;
        }
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
        let outcome = CliGit.run_custom(&repo.loc(), "(sleep 30; echo late) & echo done", true);
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
        ) {
            CustomOutcome::Timeout => {}
            other => panic!("expected timeout, got {other:?}"),
        }
        std::thread::sleep(Duration::from_secs(2));
        assert!(!marker.exists(), "descendant survived group termination");
        let (kind, status, stdout, _, _) = CliGit.run_custom(&repo.loc(), "echo ok", true);
        assert_eq!(kind, CUSTOM_OUTCOME_EXITED);
        assert!(status.is_some_and(|status| status.success()));
        assert_eq!(stdout.trim(), "ok");
    }
}
