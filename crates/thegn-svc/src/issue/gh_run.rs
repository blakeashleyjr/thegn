//! Bounded subprocess runner for CLI-backed issue providers.
//!
//! The caller hands over a ready [`Command`]; this module owns everything that
//! makes running it safe on a service worker: a dedicated process group (so a
//! hung credential helper's children die with it), a wall-clock deadline,
//! per-stream byte caps, a process-wide concurrency bound, cancellation when the
//! awaiting future is dropped, and a guaranteed reap. The blocking part runs on
//! `spawn_blocking`, never on an async worker.

use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::sync::Semaphore;

use crate::plugin::proc::bounded_group_termination_supported;

/// Limits for one run.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub timeout: Duration,
    /// Cap on captured stdout bytes.
    pub max_stdout: usize,
    /// Cap on captured stderr bytes. Non-streaming runs keep the newest bytes;
    /// the streaming log runner keeps the first. In either case, excess bytes
    /// are drained and discarded and do not cause an error.
    pub max_stderr: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            timeout: Duration::from_secs(30),
            max_stdout: 8 * 1024 * 1024,
            max_stderr: 64 * 1024,
        }
    }
}

/// Max `gh` children alive at once, process-wide.
const MAX_CONCURRENT: usize = 4;

fn permits() -> &'static Arc<Semaphore> {
    static S: std::sync::OnceLock<Arc<Semaphore>> = std::sync::OnceLock::new();
    S.get_or_init(|| Arc::new(Semaphore::new(MAX_CONCURRENT)))
}

#[derive(Debug)]
pub enum RunError {
    /// The program could not be started because it does not exist.
    NotInstalled,
    /// Any other spawn/IO failure.
    Io(String),
    /// Deadline passed; the process group was killed (or, if a descendant
    /// escaped the group and holds the pipes, the readers were detached).
    Timeout,
    /// Stdout exceeded its cap; the process group was killed.
    Truncated,
    /// Output pipes stayed open past the deadline (a descendant escaped the
    /// process group); the readers were detached.
    /// The awaiting future was dropped before the run finished.
    Cancelled,
    /// Non-zero exit. `stderr` is raw (callers redact it).
    Exit { code: Option<i32>, stderr: String },
}

/// Run `cmd` to completion under `limits`; returns captured stdout.
pub async fn run(cmd: Command, limits: Limits) -> Result<String, RunError> {
    if !bounded_group_termination_supported() {
        return Err(RunError::Io(
            "bounded subprocess execution is not supported on this platform".into(),
        ));
    }
    let _permit = permits()
        .clone()
        .acquire_owned()
        .await
        .map_err(|_| RunError::Cancelled)?;
    let cancel = Arc::new(AtomicBool::new(false));
    struct Guard(Arc<AtomicBool>);
    impl Drop for Guard {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Relaxed);
        }
    }
    let _guard = Guard(cancel.clone());
    // The permit stays held until the blocking task finishes even if this
    // future is dropped: the permit itself moves into the task.
    let task_permit = _permit;
    tokio::task::spawn_blocking(move || {
        let _permit = task_permit;
        run_blocking(cmd, limits, &cancel)
    })
    .await
    .map_err(|e| RunError::Io(e.to_string()))?
}

/// How long the readers get to hit EOF once the process group is dead.
const READER_GRACE: Duration = Duration::from_millis(500);

pub(crate) fn run_blocking(
    cmd: Command,
    limits: Limits,
    cancel: &AtomicBool,
) -> Result<String, RunError> {
    if cancel.load(Ordering::Relaxed) {
        return Err(RunError::Cancelled);
    }
    let mut cmd = cmd;
    let limits = thegn_proc::Limits {
        quiesce_after_leader: Some(Duration::ZERO),
        reader_grace: READER_GRACE,
        stdout: thegn_proc::Stream::HeadOrFail {
            cap: limits.max_stdout,
        },
        stderr: thegn_proc::Stream::Tail {
            cap: limits.max_stderr,
        },
        ..thegn_proc::Limits::silent(limits.timeout)
    };
    map_run(thegn_proc::run(&mut cmd, &limits, cancel))
}

fn map_run(run: thegn_proc::Run) -> Result<String, RunError> {
    let status = map_terminal_run(&run)?;
    if run.stdout.truncated {
        return Err(RunError::Truncated);
    }
    if status.success() {
        Ok(String::from_utf8_lossy(&run.stdout.bytes).into_owned())
    } else {
        Err(RunError::Exit {
            code: status.code(),
            stderr: String::from_utf8_lossy(&run.stderr.bytes).into_owned(),
        })
    }
}

fn map_terminal_run(run: &thegn_proc::Run) -> Result<std::process::ExitStatus, RunError> {
    use thegn_proc::{End, GroupState, Leader};

    if run.group == GroupState::Survivors {
        tracing::debug!("gh subprocess group had survivors after termination");
    }
    match &run.end {
        End::Spawn(error) => {
            if error.kind() == std::io::ErrorKind::NotFound {
                Err(RunError::NotInstalled)
            } else {
                Err(RunError::Io(error.to_string()))
            }
        }
        End::Supervise(error) => Err(RunError::Io(error.to_string())),
        End::Cancelled => Err(RunError::Cancelled),
        End::OutputLimit(_) => Err(RunError::Truncated),
        End::TimedOut => Err(RunError::Timeout),
        End::LeaderExited => {
            let status = match &run.leader {
                Leader::Reaped(status) => status.clone(),
                Leader::ReapFailed(error) => return Err(RunError::Io(error.to_string())),
                Leader::Detached | Leader::NotSpawned => {
                    return Err(RunError::Io("could not reap child".into()));
                }
            };
            if !run.stdout.eof || !run.stderr.eof {
                return Err(RunError::Timeout);
            }
            Ok(status)
        }
    }
}

/// Run a streaming log command through the shared bounded subprocess runner.
pub(crate) fn run_blocking_streaming(
    mut cmd: Command,
    limits: Limits,
    cancel: &AtomicBool,
    consume: &mut dyn FnMut(&[u8]),
) -> Result<std::process::ExitStatus, RunError> {
    if cancel.load(Ordering::Relaxed) {
        return Err(RunError::Cancelled);
    }
    let run_limits = thegn_proc::Limits {
        quiesce_after_leader: Some(Duration::ZERO),
        reader_grace: READER_GRACE,
        stdout: thegn_proc::Stream::Consume,
        stderr: thegn_proc::Stream::Head {
            cap: limits.max_stderr,
        },
        ..thegn_proc::Limits::silent(limits.timeout)
    };
    let run = thegn_proc::run_consuming(&mut cmd, &run_limits, cancel, consume);
    let status = map_terminal_run(&run)?;
    if status.success() {
        Ok(status)
    } else {
        Err(RunError::Exit {
            code: status.code(),
            stderr: String::from_utf8_lossy(&run.stderr.bytes).into_owned(),
        })
    }
}

/// Strip credential-looking material from CLI diagnostics and cap the length,
/// keeping the first useful line(s).
pub fn redact(stderr: &str) -> String {
    const MAX: usize = 400;
    let mut out = String::new();
    for line in stderr.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let lower = line.to_ascii_lowercase();
        if lower.contains("authorization:") || lower.contains("bearer ") {
            continue;
        }
        let cleaned: Vec<String> = line
            .split_whitespace()
            .map(|w| {
                if word_is_secret(w) {
                    "<redacted>".to_owned()
                } else {
                    w.to_owned()
                }
            })
            .collect();
        if !out.is_empty() {
            out.push_str("; ");
        }
        out.push_str(&cleaned.join(" "));
        if out.len() >= MAX {
            break;
        }
    }
    if out.len() > MAX {
        let mut end = MAX;
        while !out.is_char_boundary(end) {
            end -= 1;
        }
        out.truncate(end);
        out.push('…');
    }
    out
}

/// A whitespace-delimited word that carries credential material anywhere in
/// it: a GitHub token prefix (`GH_TOKEN=ghp_x`, `token:ghp_x`, JSON
/// `"token":"ghp_x"`), `token=`, or `user:pass@host` credentials with or
/// without a scheme.
fn word_is_secret(w: &str) -> bool {
    const PREFIXES: [&str; 6] = ["ghp_", "gho_", "ghs_", "ghu_", "ghr_", "github_pat_"];
    let lower = w.to_ascii_lowercase();
    if PREFIXES.iter().any(|p| lower.contains(p)) || lower.starts_with("token=") {
        return true;
    }
    match w.find('@') {
        Some(at) => w.contains("://") || w[..at].contains(':'),
        None => false,
    }
}

#[cfg(all(test, unix))]
mod unix_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_strips_tokens_and_caps() {
        let r =
            redact("error using ghp_abcdef123 and https://u:p@host/x\nAuthorization: Bearer zzz");
        assert!(
            !r.contains("ghp_") && !r.contains("u:p@") && !r.contains("zzz"),
            "{r}"
        );
        assert!(r.contains("error using"));
        assert!(redact(&"a ".repeat(1000)).chars().count() <= 401);
    }

    #[test]
    fn redact_covers_embedded_and_schemeless_shapes() {
        for bad in [
            "GH_TOKEN=ghp_secret1",
            "token:ghp_secret2",
            "{\"token\":\"ghp_secret3\"}",
            "x-access-token:ghs_secret4@github.com",
            "user:hunter2@host.example",
            "see github_pat_secret5.",
            "ghr_secret6,",
            "(gho_secret7)",
            "TOKEN=plain",
        ] {
            let r = redact(&format!("failed {bad} now"));
            assert!(r.contains("<redacted>"), "{bad} -> {r}");
            assert!(
                !r.contains("secret") && !r.contains("hunter2") && !r.contains("plain"),
                "{bad} -> {r}"
            );
            assert!(r.contains("failed") && r.contains("now"), "{r}");
        }
        // Not credentials.
        let ok = redact("git@github.com: permission denied for o/r");
        assert!(!ok.contains("<redacted>"), "{ok}");
    }
}
