//! Bounded subprocess runner for CLI-backed issue providers.
//!
//! The caller hands over a ready [`Command`]; this module owns everything that
//! makes running it safe on a service worker: a dedicated process group (so a
//! hung credential helper's children die with it), a wall-clock deadline,
//! per-stream byte caps, a process-wide concurrency bound, cancellation when the
//! awaiting future is dropped, and a guaranteed reap. The blocking part runs on
//! `spawn_blocking`, never on an async worker.

use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use tokio::sync::Semaphore;

use crate::plugin::proc::{
    bounded_group_termination_supported, kill_group, leader_exited_nowait, set_process_group,
};

/// Limits for one run.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub timeout: Duration,
    /// Cap on captured stdout bytes.
    pub max_stdout: usize,
    /// Cap on captured stderr bytes (stderr beyond it is read and discarded,
    /// not an error).
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

/// Read to EOF, keeping at most `cap` bytes. Bytes past the cap are still
/// read and discarded so the child never sees a closed pipe (EPIPE/SIGPIPE);
/// `over` is set when anything was dropped.
fn read_capped(mut r: impl Read, cap: usize, over: &AtomicBool) -> Vec<u8> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match r.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let room = cap.saturating_sub(buf.len());
                if n > room {
                    over.store(true, Ordering::Relaxed);
                }
                buf.extend_from_slice(&chunk[..n.min(room)]);
            }
        }
    }
    buf
}

/// How long the readers get to hit EOF once the process group is dead.
const READER_GRACE: Duration = Duration::from_millis(500);

fn set_io_failure_if_missing(failure: &mut Option<RunError>, error: std::io::Error) {
    if failure.is_none() {
        *failure = Some(RunError::Io(error.to_string()));
    }
}

pub(crate) fn run_blocking(
    cmd: Command,
    limits: Limits,
    cancel: &AtomicBool,
) -> Result<String, RunError> {
    let out_over = Arc::new(AtomicBool::new(false));
    let reader_over = out_over.clone();
    let (stdout, _) =
        run_blocking_core(cmd, limits, cancel, Some(out_over), false, move |reader| {
            read_capped(reader, limits.max_stdout, &reader_over)
        })?;
    Ok(String::from_utf8_lossy(&stdout).into_owned())
}

/// Consume stdout on its reader thread. The consumer owns retention; after it
/// returns, the runner drains and discards the remaining pipe bytes.
#[allow(dead_code)] // The batch-21 ci.rs caller is deferred until that lane lands.
pub(crate) fn run_blocking_streaming<T, F>(
    cmd: Command,
    limits: Limits,
    cancel: &AtomicBool,
    consume: F,
) -> Result<(T, std::process::ExitStatus), RunError>
where
    F: FnOnce(&mut dyn Read) -> T + Send + 'static,
    T: Send + 'static,
{
    run_blocking_core(cmd, limits, cancel, None, true, consume)
}

enum ReaderMessage<T> {
    Stdout(Result<T, String>),
    Stderr(Result<Vec<u8>, String>),
}

fn run_blocking_core<T, F>(
    mut cmd: Command,
    limits: Limits,
    cancel: &AtomicBool,
    out_over: Option<Arc<AtomicBool>>,
    strict_reader_errors: bool,
    consume: F,
) -> Result<(T, std::process::ExitStatus), RunError>
where
    F: FnOnce(&mut dyn Read) -> T + Send + 'static,
    T: Send + 'static,
{
    if !bounded_group_termination_supported() {
        return Err(RunError::Io(
            "bounded subprocess execution is not supported on this platform".into(),
        ));
    }
    if cancel.load(Ordering::Relaxed) {
        return Err(RunError::Cancelled);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    set_process_group(&mut cmd);
    let mut child = cmd.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            RunError::NotInstalled
        } else {
            RunError::Io(e.to_string())
        }
    })?;
    let pid = child.id();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let (tx, rx) = mpsc::channel::<ReaderMessage<T>>();
    let tx_out = tx.clone();
    std::thread::spawn(move || {
        let result = match stdout {
            Some(mut stream) => {
                let value = consume(&mut stream);
                match std::io::copy(&mut stream, &mut std::io::sink()) {
                    Ok(_) => Ok(value),
                    Err(error) if strict_reader_errors => Err(error.to_string()),
                    Err(_) => Ok(value),
                }
            }
            None => Err("stdout pipe was not available".to_owned()),
        };
        if tx_out.send(ReaderMessage::Stdout(result)).is_err() {
            tracing::debug!("gh stdout reader detached: run already gave up");
        }
    });
    std::thread::spawn(move || {
        let result = match stderr {
            Some(stream) if strict_reader_errors => read_capped_result(stream, limits.max_stderr),
            Some(stream) => Ok(read_capped(
                stream,
                limits.max_stderr,
                &AtomicBool::new(false),
            )),
            None => Err("stderr pipe was not available".to_owned()),
        };
        if tx.send(ReaderMessage::Stderr(result)).is_err() {
            tracing::debug!("gh stderr reader detached: run already gave up");
        }
    });

    let deadline = Instant::now() + limits.timeout;
    let mut failure: Option<RunError> = None;
    // The leader is observed with WNOWAIT and reaped only after the group kill,
    // so its unreaped zombie pins the pid/pgid against reuse.
    let mut pinned = true;
    loop {
        match leader_exited_nowait(pid) {
            Ok(true) => break,
            Ok(false) => {}
            Err(e) => {
                failure = Some(RunError::Io(e.to_string()));
                pinned = false;
                break;
            }
        }
        if cancel.load(Ordering::Relaxed) {
            failure = Some(RunError::Cancelled);
            break;
        }
        if out_over
            .as_ref()
            .is_some_and(|over| over.load(Ordering::Relaxed))
        {
            failure = Some(RunError::Truncated);
            break;
        }
        if Instant::now() >= deadline {
            failure = Some(RunError::Timeout);
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    // Whole tree goes in every case: on success it reaps stragglers that kept
    // the pipes open. Skipped only when we lost track of the leader.
    if pinned {
        kill_group(pid);
    }
    let status = if pinned {
        match child.wait() {
            Ok(status) => Some(status),
            Err(error) => {
                set_io_failure_if_missing(&mut failure, error);
                None
            }
        }
    } else {
        match child.try_wait() {
            Ok(status) => status,
            Err(error) => {
                set_io_failure_if_missing(&mut failure, error);
                None
            }
        }
    };
    if let Some(f) = failure {
        return Err(f);
    }
    // Bounded collection: a descendant that escaped the group (setsid) can
    // hold the pipes open forever; detach the readers instead of joining.
    let drain_by = Instant::now() + READER_GRACE;
    let (mut stdout, mut stderr) = (None, None);
    while stdout.is_none() || stderr.is_none() {
        let left = drain_by.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(ReaderMessage::Stdout(value)) => stdout = Some(value),
            Ok(ReaderMessage::Stderr(value)) => stderr = Some(value),
            Err(_) => return Err(RunError::Timeout),
        }
    }
    let stdout = stdout
        .expect("stdout reader completed")
        .map_err(RunError::Io)?;
    let stderr = stderr
        .expect("stderr reader completed")
        .map_err(RunError::Io)?;
    if out_over
        .as_ref()
        .is_some_and(|over| over.load(Ordering::Relaxed))
    {
        return Err(RunError::Truncated);
    }
    match status {
        Some(status) if status.success() => Ok((stdout, status)),
        Some(status) => Err(RunError::Exit {
            code: status.code(),
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
        }),
        None => Err(RunError::Io("could not reap child".into())),
    }
}

fn read_capped_result(mut reader: impl Read, cap: usize) -> Result<Vec<u8>, String> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match reader.read(&mut chunk) {
            Ok(0) => return Ok(buf),
            Ok(n) => {
                let room = cap.saturating_sub(buf.len());
                buf.extend_from_slice(&chunk[..n.min(room)]);
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error.to_string()),
        }
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
