pub use std::process::ExitStatus;
use std::{io, process::Command, sync::atomic::AtomicBool, time::Duration};

#[cfg(not(unix))]
mod fallback;
#[cfg(test)]
mod platform_ratchet_tests;
#[cfg(unix)]
mod unix;

/// First drain/supervise poll interval; doubles up to BACKOFF_CAP.
pub const POLL_INTERVAL: Duration = Duration::from_millis(10);
/// Ceiling of the supervise/drain backoff (each liveness check is a /proc scan on Linux).
pub const BACKOFF_CAP: Duration = Duration::from_millis(250);
/// Doubles `current`, never above BACKOFF_CAP, never above `remaining`.
pub fn next_backoff(current: Duration, remaining: Duration) -> Duration {
    current.saturating_mul(2).min(BACKOFF_CAP).min(remaining)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    /// `Stdio::null()`; never read.
    Null,
    /// Piped and drained to EOF for the whole run; keep only the first `cap` bytes. Never ends the run.
    Head { cap: usize },
    /// Piped; keep the FIRST bytes. More than `cap` bytes ends the run: End::OutputLimit, group terminated.
    HeadOrFail { cap: usize },
    /// Piped and drained to EOF for the whole run; keep only the newest `cap` bytes. Never ends the run.
    Tail { cap: usize },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Which {
    Stdout,
    Stderr,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limits {
    /// Execution deadline measured from spawn. A leader still running at it => End::TimedOut.
    pub timeout: Duration,
    /// After the leader exits (left unreaped): how long other group members may keep running before
    /// the group is terminated. None = until `timeout`; Some(Duration::ZERO) = terminate at once.
    pub quiesce_after_leader: Option<Duration>,
    /// SIGTERM -> SIGKILL grace when terminating. Duration::ZERO = SIGKILL straight away.
    pub term_grace: Duration,
    /// After SIGKILL: bound for the group to empty, and (separately) for the leader to become reapable.
    pub kill_settle: Duration,
    /// After group settlement: how long still-open pipes may run to EOF before they are closed.
    pub reader_grace: Duration,
    pub stdout: Stream,
    pub stderr: Stream,
    /// Cap across both Tail streams (older bytes of the larger tail trimmed first). None = no aggregate cap.
    pub aggregate_tail_cap: Option<usize>,
}
impl Limits {
    /// No pipes: timeout = `timeout`, quiesce None, term_grace ZERO, kill_settle 250 ms,
    /// reader_grace ZERO, stdout/stderr Null, aggregate None. Callers adjust with
    /// `Limits { field: v, ..Limits::silent(t) }` (never assign fields after construction).
    pub fn silent(timeout: Duration) -> Self {
        Self {
            timeout,
            quiesce_after_leader: None,
            term_grace: Duration::ZERO,
            kill_settle: Duration::from_millis(250),
            reader_grace: Duration::ZERO,
            stdout: Stream::Null,
            stderr: Stream::Null,
            aggregate_tail_cap: None,
        }
    }
}

/// Why supervision stopped.
#[derive(Debug)]
pub enum End {
    /// The leader exited on its own (status in `Run::leader`).
    LeaderExited,
    TimedOut,
    /// The cancel flag was seen (before the leader exited, or during the post-exit quiesce).
    Cancelled,
    OutputLimit(Which),
    /// `Command::spawn` failed; nothing ran. Callers read `.kind()` (e.g. NotFound).
    Spawn(io::Error),
    /// Observing the leader (waitid) failed: the pgid is no longer pinned, so the group was NOT signalled.
    Supervise(io::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupState {
    /// Every member exited on its own; no signal was sent.
    Drained,
    /// Terminated (TERM and/or KILL) and observed empty within term_grace / kill_settle.
    Terminated,
    /// Members outlived SIGKILL + kill_settle (uninterruptible sleep, or not observable).
    Survivors,
    /// Not observed and not signalled (End::Spawn or End::Supervise).
    Unknown,
}

#[derive(Debug)]
pub enum Leader {
    Reaped(ExitStatus),
    ReapFailed(io::Error),
    /// Not reapable within kill_settle after SIGKILL; a one-shot named thread `thegn-proc-reap` owns
    /// and waits it (inline blocking wait if that thread cannot be spawned). Never a zombie.
    Detached,
    NotSpawned,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Capture {
    /// Head (Head/HeadOrFail) or tail (Tail) bytes; empty for Null.
    pub bytes: Vec<u8>,
    /// Every byte read, retained or not.
    pub total: u64,
    /// Some bytes were dropped (Head, Tail trimming, aggregate fit) or the HeadOrFail cap was exceeded.
    pub truncated: bool,
    /// The pipe reached EOF (false: closed by us after reader_grace, a holder escaped the group, or Null).
    pub eof: bool,
}

fn aggregate_fit(
    stdout: &mut Capture,
    stderr: &mut Capture,
    stdout_stream: Stream,
    stderr_stream: Stream,
    cap: Option<usize>,
) {
    let Some(cap) = cap else { return };
    let stdout_tail = matches!(stdout_stream, Stream::Tail { .. });
    let stderr_tail = matches!(stderr_stream, Stream::Tail { .. });
    while stdout.bytes.len() + stderr.bytes.len() > cap && (stdout_tail || stderr_tail) {
        let trim_stdout = match (
            stdout_tail && !stdout.bytes.is_empty(),
            stderr_tail && !stderr.bytes.is_empty(),
        ) {
            (true, true) => stdout.bytes.len() > stderr.bytes.len(),
            (true, false) => true,
            (false, true) => false,
            (false, false) => break,
        };
        if trim_stdout {
            let excess = (stdout.bytes.len() + stderr.bytes.len() - cap).min(stdout.bytes.len());
            stdout.bytes.drain(..excess);
            stdout.truncated = true;
        } else {
            let excess = (stdout.bytes.len() + stderr.bytes.len() - cap).min(stderr.bytes.len());
            stderr.bytes.drain(..excess);
            stderr.truncated = true;
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Containment {
    /// Own process group; leader pinned as an unreaped zombie until settlement (unix).
    ProcessGroup,
    /// Direct child only (non-unix until THE-274 Job Objects).
    DirectChildOnly,
}

#[derive(Debug)]
pub struct Run {
    pub end: End,
    pub group: GroupState,
    pub leader: Leader,
    /// True when the leader exited on its own before any signal (also when End::Cancelled /
    /// OutputLimit came during the quiesce window afterwards).
    pub leader_exited_first: bool,
    pub stdout: Capture,
    pub stderr: Capture,
    pub containment: Containment,
}
impl Run {
    /// End::LeaderExited and Leader::Reaped(s) with s.success().
    pub fn success(&self) -> bool {
        matches!(&self.end, End::LeaderExited)
            && matches!(&self.leader, Leader::Reaped(status) if status.success())
    }
    /// The reaped leader status, if any.
    pub fn exit_status(&self) -> Option<ExitStatus> {
        match &self.leader {
            Leader::Reaped(status) => Some(*status),
            _ => None,
        }
    }
}

/// Spawn `cmd` and run it to a bounded, fully reaped end. Overrides stdin (always null) and
/// stdout/stderr (per `limits`); everything else (args, env, cwd) is the caller's. Unix: own
/// process group (`process_group(0)`). Blocking: call it from a worker thread, never the UI loop.
/// Wall time <= timeout + term_grace + 2 * kill_settle + reader_grace + one BACKOFF_CAP.
pub fn run(cmd: &mut Command, limits: &Limits, cancel: &AtomicBool) -> Run {
    #[cfg(unix)]
    {
        unix::run(cmd, limits, cancel)
    }
    #[cfg(not(unix))]
    {
        fallback::run(cmd, limits, cancel)
    }
}
