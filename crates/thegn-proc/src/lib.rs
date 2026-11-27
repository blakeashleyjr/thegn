pub use std::process::ExitStatus;
use std::{
    io,
    process::{ChildStdin, Command},
    sync::atomic::AtomicBool,
    time::Duration,
};

#[cfg(not(unix))]
mod fallback;
#[cfg(test)]
mod platform_ratchet_tests;
#[cfg(unix)]
mod unix;

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub use unix::group_has_live_members;

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
/// Return whether the process group has a live member.
///
/// Platforms without a process-group liveness probe conservatively report
/// that the group is live.
pub fn group_has_live_members(_pgid: u32) -> bool {
    true
}

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
    /// Piped and drained to EOF for the whole run; every byte of this stream is handed, in order,
    /// to the consumer passed to [`run_consuming_streams`] (or, for stdout only,
    /// [`run_consuming`]) on the calling thread. Nothing is retained (`Capture::bytes` stays
    /// empty, `total` counts every byte). Never ends the run. Under [`run_consuming`] stderr
    /// drains and drops.
    Consume,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Which {
    Stdout,
    Stderr,
}

/// What happens to the rest of the process group when the leader exits on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LeaderExit {
    /// Wait out `Limits::quiesce_after_leader` (or the timeout) for the other members, then
    /// terminate whoever is left.
    #[default]
    Quiesce,
    /// When the leader exits on its own, the group is not signalled. The leader is reaped,
    /// still-open pipes get `reader_grace`; after it their output is read and discarded until
    /// EOF by a `thegn-proc-sink` thread, so survivors can keep writing, and `group` is
    /// [`GroupState::Detached`] if members remain ([`GroupState::Drained`] if none). Timeout,
    /// cancel and output-limit ends seen while the leader runs still terminate the group (fail
    /// closed). `quiesce_after_leader` is ignored.
    Detach,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limits {
    /// Execution deadline measured from spawn. A leader still running at it => End::TimedOut.
    pub timeout: Duration,
    /// After the leader exits (left unreaped): how long other group members may keep running before
    /// the group is terminated. None = until `timeout`; Some(Duration::ZERO) = terminate at once.
    pub quiesce_after_leader: Option<Duration>,
    /// Quiesce (default) or leave the remaining group members running; see [`LeaderExit`].
    pub leader_exit: LeaderExit,
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
    /// After this long with no byte read on any piped stream (Head/HeadOrFail/Tail/Consume),
    /// terminate the group as for `timeout` (End::TimedOut with `Run::idle_expired`). The clock
    /// starts at spawn (a child that never writes expires after this span), resets on every chunk
    /// read, and stops once the leader exits (the quiesce/reader-grace phases keep their own
    /// bounds). None = no idle bound.
    pub idle_output: Option<Duration>,
}
impl Limits {
    /// No pipes: timeout = `timeout`, quiesce None, leader_exit Quiesce, term_grace ZERO, kill_settle 250 ms,
    /// reader_grace ZERO, stdout/stderr Null, aggregate None, idle_output None. Callers adjust with
    /// `Limits { field: v, ..Limits::silent(t) }` (never assign fields after construction).
    pub fn silent(timeout: Duration) -> Self {
        Self {
            timeout,
            quiesce_after_leader: None,
            leader_exit: LeaderExit::Quiesce,
            term_grace: Duration::ZERO,
            kill_settle: Duration::from_millis(250),
            reader_grace: Duration::ZERO,
            stdout: Stream::Null,
            stderr: Stream::Null,
            aggregate_tail_cap: None,
            idle_output: None,
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
    /// The leader exited and other members were deliberately left running; not signalled
    /// ([`LeaderExit::Detach`]).
    Detached,
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
    /// Head (Head/HeadOrFail) or tail (Tail) bytes; empty for Null and Consume.
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
    /// Own process group (unix). The supervisor proves emptiness using platform process state
    /// while retaining a safe group identity for signalling; Linux uses reap-then-probe plus a
    /// `/proc` fallback, while macOS excludes zombie-only members through libproc.
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
    /// True when the run ended because `Limits::idle_output` elapsed; `end` is then End::TimedOut.
    pub idle_expired: bool,
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

    /// The typed failure, if this run did not succeed.
    ///
    /// Invariant: `error().is_none() == success()`.
    pub fn error(&self) -> Option<ProcError> {
        match (&self.end, &self.leader) {
            (End::Spawn(error), _) => Some(ProcError::Spawn {
                kind: error.kind(),
                message: error.to_string(),
            }),
            (End::Supervise(error), _) => Some(ProcError::Supervise {
                message: error.to_string(),
            }),
            (End::TimedOut, _) => Some(ProcError::TimedOut),
            (End::Cancelled, _) => Some(ProcError::Cancelled),
            (End::OutputLimit(which), _) => Some(ProcError::OutputLimit(*which)),
            (End::LeaderExited, Leader::Reaped(status)) if status.success() => None,
            (End::LeaderExited, Leader::Reaped(status)) => Some(ProcError::NonZero(*status)),
            (End::LeaderExited, Leader::ReapFailed(error)) => Some(ProcError::Reap {
                message: error.to_string(),
            }),
            (End::LeaderExited, Leader::Detached | Leader::NotSpawned) => Some(ProcError::Unreaped),
        }
    }
}

/// A typed failure from a bounded subprocess run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcError {
    Spawn {
        kind: io::ErrorKind,
        message: String,
    },
    TimedOut,
    Cancelled,
    OutputLimit(Which),
    Supervise {
        message: String,
    },
    NonZero(ExitStatus),
    Reap {
        message: String,
    },
    Unreaped,
}

impl std::fmt::Display for ProcError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Spawn { message, .. } => write!(formatter, "could not run: {message}"),
            Self::TimedOut => formatter.write_str("timed out"),
            Self::Cancelled => formatter.write_str("cancelled"),
            Self::OutputLimit(Which::Stdout) => formatter.write_str("stdout exceeded its byte cap"),
            Self::OutputLimit(Which::Stderr) => formatter.write_str("stderr exceeded its byte cap"),
            Self::Supervise { message } => {
                write!(formatter, "could not observe the process: {message}")
            }
            Self::NonZero(status) => write!(formatter, "exited with {status}"),
            Self::Reap { message } => write!(formatter, "could not reap: {message}"),
            Self::Unreaped => formatter.write_str("was not reaped within the kill budget"),
        }
    }
}

impl std::error::Error for ProcError {}

/// Spawn `cmd` and run it to a bounded, fully reaped end. Overrides stdin (always null; use
/// [`run_with_input`] for a payload) and stdout/stderr (per `limits`); everything else (args, env,
/// cwd) is the caller's. Unix: own
/// process group (`process_group(0)`). Blocking: call it from a worker thread, never the UI loop.
/// Wall time <= timeout + term_grace + 2 * kill_settle + reader_grace + one BACKOFF_CAP.
pub fn run(cmd: &mut Command, limits: &Limits, cancel: &AtomicBool) -> Run {
    run_consuming(cmd, limits, cancel, &mut |_bytes: &[u8]| {})
}

/// Spawn `cmd` and run it to a bounded, fully reaped end, passing each stdout chunk to `consume`
/// on the calling thread. Overrides stdin (always null; use [`run_with_input`] for a payload) and
/// stdout/stderr (per `limits`); everything else (args, env, cwd) is the caller's. Blocking: call
/// it from a worker thread, never the UI loop. Wall time <= timeout + term_grace + 2 * kill_settle
/// + reader_grace + one BACKOFF_CAP.
pub fn run_consuming(
    cmd: &mut Command,
    limits: &Limits,
    cancel: &AtomicBool,
    consume: &mut dyn FnMut(&[u8]),
) -> Run {
    run_consuming_streams(cmd, limits, cancel, &mut |which: Which, bytes: &[u8]| {
        if which == Which::Stdout {
            consume(bytes);
        }
    })
}

/// A consumer of [`Stream::Consume`] bytes, told which stream each chunk came from.
pub type StreamConsumer<'a> = &'a mut dyn FnMut(Which, &[u8]);

/// Like [`run_consuming`], but every [`Stream::Consume`] stream (stdout, stderr or both) hands
/// its bytes to `consume(which, bytes)` on the calling thread, in order within each stream.
/// Overrides stdin (always null; use [`run_with_input`] for a payload) and stdout/stderr (per
/// `limits`); everything else (args, env, cwd) is the caller's. Blocking: call it from a worker
/// thread, never the UI loop. Wall time <= timeout + term_grace + 2 * kill_settle + reader_grace
/// + one BACKOFF_CAP.
pub fn run_consuming_streams(
    cmd: &mut Command,
    limits: &Limits,
    cancel: &AtomicBool,
    consume: StreamConsumer<'_>,
) -> Run {
    #[cfg(unix)]
    {
        unix::run_consuming_streams(cmd, limits, cancel, consume)
    }
    #[cfg(not(unix))]
    {
        fallback::run_consuming_streams(cmd, limits, cancel, consume)
    }
}

/// Receives a run's live stdin pipe once, right after spawn. See
/// [`run_consuming_streams_with_stdin`].
pub type StdinHandoff<'a> = &'a mut dyn FnMut(ChildStdin);

/// Like [`run_consuming_streams`], but stdin is a pipe handed to the caller instead of null.
///
/// Right after a successful spawn (Unix: with the process group already set up) the runner calls
/// `on_stdin(stdin)` exactly once on the calling thread and keeps no handle of its own. On spawn
/// failure `on_stdin` is never called and the run ends exactly as [`run`] would. The runner then
/// supervises exactly like [`run_consuming_streams`]: same limits, cancel, kill escalation, reap
/// and wall-time bound (timeout + term_grace + 2 * kill_settle + reader_grace + one BACKOFF_CAP).
///
/// The caller owns stdin and its lifetime. `on_stdin` must return promptly (move the pipe into a
/// writer thread, or write a payload small enough for the pipe buffer): output is only drained
/// once it returns. The runner never waits for stdin to close, so holding it open past the run is
/// harmless. A writer must expect `BrokenPipe` (EPIPE) once the child closes stdin or the group is
/// signalled; this relies on SIGPIPE being ignored, which Rust's std main does.
pub fn run_consuming_streams_with_stdin(
    cmd: &mut Command,
    limits: &Limits,
    cancel: &AtomicBool,
    on_stdin: StdinHandoff<'_>,
    consume: StreamConsumer<'_>,
) -> Run {
    #[cfg(unix)]
    {
        unix::run_consuming_streams_with_stdin(cmd, limits, cancel, on_stdin, consume)
    }
    #[cfg(not(unix))]
    {
        fallback::run_consuming_streams_with_stdin(cmd, limits, cancel, on_stdin, consume)
    }
}

/// What a stdin payload passed to [`run_with_input`] achieved.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct InputFeed {
    /// Bytes the child's stdin pipe accepted.
    pub written: u64,
    /// Every byte was written and stdin was closed (the child saw EOF).
    pub complete: bool,
    /// Why feeding stopped early (BrokenPipe: the child closed stdin). None when complete, or when
    /// the run ended first.
    pub error: Option<io::ErrorKind>,
}

/// Like [`run`], but stdin is a pipe fed with `input` and closed once it is written (an empty `input`
/// closes it at once). Unix: written non-blocking from the same poll(2) supervision loop (no extra
/// thread); stdin is closed before the group is signalled, so a child that never reads cannot hold
/// the run past `limits`. Relies on SIGPIPE being ignored (Rust's std main does this), so a closed
/// pipe is an EPIPE error, not a signal.
pub fn run_with_input(
    cmd: &mut Command,
    limits: &Limits,
    input: &[u8],
    cancel: &AtomicBool,
) -> (Run, InputFeed) {
    #[cfg(unix)]
    {
        unix::run_with_input(cmd, limits, input, cancel)
    }
    #[cfg(not(unix))]
    {
        fallback::run_with_input(cmd, limits, input, cancel)
    }
}
