//! Bounded subprocess primitive for the media helpers (`playerctl`, `osascript`,
//! the MediaRemote adapter).
//!
//! Every finite helper runs through [`output`]: a hard total deadline, a byte cap
//! on each of stdout/stderr, its own process group, and deterministic cleanup.
//! The leader is only ever signalled while it is still unreaped (we never
//! `killpg` after `wait`, since the pgid could then belong to someone else), and
//! dropping the future mid-flight kills the group via [`Guard`] plus tokio's
//! `kill_on_drop` reaper. Cleanup failure is surfaced, not swallowed.

use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, Command};
use tokio::time::{Instant, timeout, timeout_at};

use crate::MediaError;

/// Hard bounds for one helper invocation.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Limits {
    /// Total wall-clock budget: spawn to exit, output fully drained.
    pub deadline: Duration,
    /// Maximum bytes captured per stream (stdout and stderr each).
    pub max_bytes: usize,
}

impl Limits {
    /// Control and read operations: generous for a slow app, bounded for a wedge.
    pub const OP: Limits = Limits {
        deadline: Duration::from_secs(10),
        max_bytes: 1 << 20,
    };
    /// Availability probes (`playerctl --version`): tiny output, short fuse.
    pub const PROBE: Limits = Limits {
        deadline: Duration::from_secs(3),
        max_bytes: 4 << 10,
    };
}

/// How long to wait for a killed tree to be reaped before reporting failure.
const REAP_BOUND: Duration = Duration::from_secs(2);

/// Typed failure of a bounded helper run.
#[derive(Debug)]
pub(crate) enum HelperError {
    Spawn(std::io::Error),
    Timeout(Duration),
    Oversize {
        stream: &'static str,
        cap: usize,
    },
    Io(std::io::Error),
    /// The helper was killed but did not reap within [`REAP_BOUND`].
    Cleanup(String),
}

impl HelperError {
    /// Map onto the backend error vocabulary, naming the helper.
    pub(crate) fn into_media(self, what: &str) -> MediaError {
        match self {
            HelperError::Spawn(e) => MediaError::Unavailable(format!("{what}: {e}")),
            HelperError::Timeout(d) => {
                MediaError::Unavailable(format!("{what}: timed out after {}s", d.as_secs()))
            }
            HelperError::Oversize { stream, cap } => {
                MediaError::Backend(format!("{what}: {stream} exceeded {cap} bytes"))
            }
            HelperError::Io(e) => MediaError::Backend(format!("{what}: {e}")),
            HelperError::Cleanup(m) => MediaError::Backend(format!("{what}: cleanup failed: {m}")),
        }
    }
}

/// Captured result of a helper that ran to completion.
#[derive(Debug)]
pub(crate) struct Captured {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// Owns the child for the duration of a run. If the owning future is dropped
/// before the leader is reaped, the whole group is killed; `kill_on_drop` then
/// reaps the leader in the background.
struct Guard(Child);

impl Guard {
    fn signal_tree(&mut self) {
        if let Some(pid) = self.0.id() {
            crate::platform::kill_group(pid);
        }
        // best-effort: the child may already have exited
        let _ = self.0.start_kill();
    }

    async fn terminate(&mut self) -> Result<(), HelperError> {
        self.signal_tree();
        match timeout(REAP_BOUND, self.0.wait()).await {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(e)) => Err(HelperError::Cleanup(e.to_string())),
            Err(_) => Err(HelperError::Cleanup("reap timed out".into())),
        }
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        self.signal_tree();
    }
}

async fn read_capped<R: AsyncRead + Unpin>(
    pipe: Option<R>,
    cap: usize,
    stream: &'static str,
) -> Result<Vec<u8>, HelperError> {
    let Some(mut pipe) = pipe else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = pipe.read(&mut buf).await.map_err(HelperError::Io)?;
        if n == 0 {
            return Ok(out);
        }
        if out.len() + n > cap {
            return Err(HelperError::Oversize { stream, cap });
        }
        out.extend_from_slice(&buf[..n]);
    }
}

/// Run `cmd` to completion under `limits`. stdin is closed; both output pipes
/// are captured up to `limits.max_bytes`. On timeout, oversize, or I/O error the
/// whole process tree is killed and reaped before returning.
pub(crate) async fn output(mut cmd: Command, limits: Limits) -> Result<Captured, HelperError> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    crate::platform::prepare_group(&mut cmd);
    let end = Instant::now() + limits.deadline;
    let mut guard = Guard(cmd.spawn().map_err(HelperError::Spawn)?);
    let out = guard.0.stdout.take();
    let err = guard.0.stderr.take();

    // Drain both pipes, then reap, all inside one deadline. The leader is
    // still unreaped on every failure path below, so killing the group is safe.
    let run = async {
        let (o, e) = tokio::try_join!(
            read_capped(out, limits.max_bytes, "stdout"),
            read_capped(err, limits.max_bytes, "stderr"),
        )?;
        let status = guard.0.wait().await.map_err(HelperError::Io)?;
        Ok::<_, HelperError>(Captured {
            status,
            stdout: o,
            stderr: e,
        })
    };
    let result = match timeout_at(end, run).await {
        Ok(r) => r,
        Err(_) => Err(HelperError::Timeout(limits.deadline)),
    };
    if result.is_err() {
        guard.terminate().await?;
    }
    result
}

#[cfg(test)]
mod tests;
