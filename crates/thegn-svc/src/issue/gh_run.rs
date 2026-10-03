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
use std::time::{Duration, Instant};

use tokio::sync::Semaphore;

use crate::plugin::proc::{kill_group, set_process_group};

/// Limits for one run.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub timeout: Duration,
    /// Cap on captured stdout bytes.
    pub max_stdout: usize,
    /// Cap on captured stderr bytes (stderr beyond it is dropped, not an error).
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
    /// Deadline passed; the process group was killed.
    Timeout,
    /// Stdout exceeded its cap; the process group was killed.
    Truncated,
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
    // future is dropped: move a clone into the task.
    let task_permit = _permit;
    tokio::task::spawn_blocking(move || {
        let _permit = task_permit;
        run_blocking(cmd, limits, &cancel)
    })
    .await
    .map_err(|e| RunError::Io(e.to_string()))?
}

fn read_capped(mut r: impl Read, cap: usize, over: &AtomicBool) -> Vec<u8> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match r.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if buf.len() + n > cap {
                    let room = cap - buf.len();
                    buf.extend_from_slice(&chunk[..room]);
                    over.store(true, Ordering::Relaxed);
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
            }
        }
    }
    buf
}

pub(crate) fn run_blocking(
    mut cmd: Command,
    limits: Limits,
    cancel: &AtomicBool,
) -> Result<String, RunError> {
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
    let o = out_over.clone();
    let out_t =
        std::thread::spawn(move || stdout.map_or_else(Vec::new, |s| read_capped(s, cap_out, &o)));
    let err_t = std::thread::spawn(move || {
        stderr.map_or_else(Vec::new, |s| read_capped(s, cap_err, &err_over))
    });

    let deadline = Instant::now() + limits.timeout;
    let mut failure: Option<RunError> = None;
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break Some(st),
            Ok(None) => {}
            Err(e) => {
                failure = Some(RunError::Io(e.to_string()));
                break None;
            }
        }
        if cancel.load(Ordering::Relaxed) {
            failure = Some(RunError::Cancelled);
            break None;
        }
        if out_over.load(Ordering::Relaxed) {
            failure = Some(RunError::Truncated);
            break None;
        }
        if Instant::now() >= deadline {
            failure = Some(RunError::Timeout);
            break None;
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    // Whole tree goes in every case: on success it reaps stragglers that kept
    // the pipes open, which would otherwise wedge the reader joins.
    kill_group(pid);
    if status.is_none() {
        // best-effort: the child is already SIGKILLed; wait only reaps it
        child.kill().ok();
    }
    let status = match status {
        Some(s) => Some(s),
        None => child.wait().ok(),
    };
    let stdout = out_t.join().unwrap_or_default();
    let stderr = err_t.join().unwrap_or_default();
    if let Some(f) = failure {
        return Err(f);
    }
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
                let t = w.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '_');
                let secret = t.starts_with("ghp_")
                    || t.starts_with("gho_")
                    || t.starts_with("ghs_")
                    || t.starts_with("ghu_")
                    || t.starts_with("ghr_")
                    || t.starts_with("github_pat_")
                    || w.contains("://") && w.contains('@')
                    || w.to_ascii_lowercase().starts_with("token=");
                if secret {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(script: &str) -> Command {
        let mut c = Command::new("sh");
        c.args(["-c", script]);
        c
    }

    fn lim(ms: u64) -> Limits {
        Limits {
            timeout: Duration::from_millis(ms),
            max_stdout: 1024,
            max_stderr: 64,
        }
    }

    #[tokio::test]
    async fn ok_captures_stdout() {
        assert_eq!(run(sh("printf hi"), lim(5000)).await.unwrap(), "hi");
    }

    #[tokio::test]
    async fn hung_process_times_out_and_group_dies() {
        let t = Instant::now();
        let r = run(sh("sleep 30 & sleep 30"), lim(150)).await;
        assert!(matches!(r, Err(RunError::Timeout)), "{r:?}");
        assert!(t.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn oversized_stdout_is_truncated_error() {
        let r = run(sh("yes x | head -c 100000; sleep 30"), lim(10_000)).await;
        assert!(matches!(r, Err(RunError::Truncated)), "{r:?}");
    }

    #[tokio::test]
    async fn nonzero_exit_carries_capped_stderr() {
        let r = run(sh("yes e | head -c 5000 >&2; exit 3"), lim(5000)).await;
        match r {
            Err(RunError::Exit { code, stderr }) => {
                assert_eq!(code, Some(3));
                assert!(stderr.len() <= 64);
            }
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn missing_program_is_not_installed() {
        let r = run(Command::new("definitely-not-a-real-gh-binary"), lim(1000)).await;
        assert!(matches!(r, Err(RunError::NotInstalled)), "{r:?}");
    }

    #[test]
    fn cancel_flag_kills_run() {
        let cancel = Arc::new(AtomicBool::new(false));
        let c2 = cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            c2.store(true, Ordering::Relaxed);
        });
        let t = Instant::now();
        let r = run_blocking(sh("sleep 30"), lim(20_000), &cancel);
        assert!(matches!(r, Err(RunError::Cancelled)), "{r:?}");
        assert!(t.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn concurrency_is_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("log");
        let script = format!("echo s >> {p}; sleep 0.3; echo e >> {p}", p = log.display());
        let mut hs = vec![];
        for _ in 0..(MAX_CONCURRENT * 2) {
            hs.push(tokio::spawn(run(sh(&script), lim(10_000))));
        }
        for h in hs {
            h.await.unwrap().unwrap();
        }
        let (mut live, mut peak) = (0i32, 0i32);
        for l in std::fs::read_to_string(&log).unwrap().lines() {
            live += if l == "s" { 1 } else { -1 };
            peak = peak.max(live);
        }
        assert!(peak <= MAX_CONCURRENT as i32, "peak {peak}");
    }

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
}
