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

pub(crate) fn run_blocking(
    mut cmd: Command,
    limits: Limits,
    cancel: &AtomicBool,
) -> Result<String, RunError> {
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
    let out_over = Arc::new(AtomicBool::new(false));
    let err_over = AtomicBool::new(false);
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let (cap_out, cap_err) = (limits.max_stdout, limits.max_stderr);
    let (tx, rx) = mpsc::channel::<(bool, Vec<u8>)>();
    let o = out_over.clone();
    let tx_out = tx.clone();
    std::thread::spawn(move || {
        let buf = stdout.map_or_else(Vec::new, |s| read_capped(s, cap_out, &o));
        if tx_out.send((true, buf)).is_err() {
            tracing::debug!("gh stdout reader detached: run already gave up");
        }
    });
    std::thread::spawn(move || {
        let buf = stderr.map_or_else(Vec::new, |s| read_capped(s, cap_err, &err_over));
        if tx.send((false, buf)).is_err() {
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
        if out_over.load(Ordering::Relaxed) {
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
        child.wait().ok()
    } else {
        child.try_wait().ok().flatten()
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
            Ok((true, b)) => stdout = Some(b),
            Ok((false, b)) => stderr = Some(b),
            Err(_) => return Err(RunError::Timeout),
        }
    }
    let (stdout, stderr) = (stdout.unwrap_or_default(), stderr.unwrap_or_default());
    if out_over.load(Ordering::Relaxed) {
        return Err(RunError::Truncated);
    }
    match status {
        Some(st) if st.success() => Ok(String::from_utf8_lossy(&stdout).into_owned()),
        Some(st) => Err(RunError::Exit {
            code: st.code(),
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
        }),
        None => Err(RunError::Io("could not reap child".into())),
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
