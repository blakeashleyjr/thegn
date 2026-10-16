//! Run a program and read newline-delimited JSON back from it.
//!
//! Every limit here exists because the program is user-supplied and may be
//! badly behaved: a runaway plugin must not be able to OOM the compositor, pin
//! a CPU, or outlive the fetch that started it.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use thegn_core::plugin_api::{Frame, RpcError, RpcErrorCode, RpcMessage, RpcResponse};

/// Longest single line accepted. A plugin that writes more than this is
/// malfunctioning, and buffering it would be the OOM.
pub const MAX_LINE_BYTES: usize = 1 << 20;
/// Most messages accepted from one run.
pub const MAX_LINES: usize = 20_000;
/// Total stdout bytes retained by a one-shot run, including kept junk lines.
pub const MAX_RUN_BYTES: usize = 8 << 20;
/// Bytes of stderr kept for the error message.
pub const MAX_STDERR: usize = 8 << 10;
/// Maximum UTF-8 bytes in a plugin RPC method name.
const MAX_METHOD_BYTES: usize = 128;
/// Maximum time to wait for a killed leader and its pipe readers to finish.
const READER_GRACE: Duration = Duration::from_millis(500);

/// Why a plugin run failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginError {
    /// The program could not be started (not on PATH, not executable).
    Spawn(String),
    /// It exited non-zero. Carries the tail of stderr — the detail
    /// `agent_run`'s discard-the-pipes approach throws away.
    Exit { code: Option<i32>, stderr: String },
    /// It ran past its timeout and its process group was killed.
    Timeout(u64),
    /// Its output was not the newline-delimited JSON we asked for.
    Protocol(String),
}

impl std::fmt::Display for PluginError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PluginError::Spawn(e) => write!(f, "could not start plugin: {e}"),
            PluginError::Exit { code, stderr } => match code {
                Some(c) => write!(f, "plugin exited {c}: {}", stderr.trim()),
                None => write!(f, "plugin was killed: {}", stderr.trim()),
            },
            PluginError::Timeout(s) => write!(f, "plugin timed out after {s}s"),
            PluginError::Protocol(e) => write!(f, "plugin protocol error: {e}"),
        }
    }
}

/// What one plugin run produced.
#[derive(Debug, Clone, Default)]
pub struct PluginRun {
    /// Messages, in the order they were written.
    pub messages: Vec<RpcMessage>,
    /// Lines that were not valid JSON. Kept rather than discarded so a plugin
    /// author can see what went wrong — a `println!` left in a script is the
    /// single most common mistake.
    pub junk: Vec<String>,
    /// Tail of stderr, capped.
    pub stderr: String,
    /// Whether output was cut off by an output limit.
    pub truncated: bool,
}

/// Consumes a plugin's stdout one line at a time, on the reader thread.
///
/// Streaming is what bounds memory: a sink that decides per line what to keep
/// never holds more than one line of undecided output, where collecting first
/// could otherwise retain [`MAX_LINES`] × [`MAX_LINE_BYTES`].
pub trait NdjsonSink: Send + 'static {
    /// One non-empty, trimmed, lossily-decoded line of at most
    /// [`MAX_LINE_BYTES`]. Return `false` to stop receiving lines; the pipe is
    /// still drained to EOF so the plugin never sees a SIGPIPE.
    fn line(&mut self, text: &str) -> bool;

    /// One non-empty line whose original bytes were not valid UTF-8. The
    /// default preserves the historical lossy behavior for other sinks.
    fn invalid_utf8(&mut self, lossy: &str) -> bool {
        self.line(lossy)
    }
}

/// Why a plugin frame was rejected by the strict one-shot decoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FrameReject {
    Syntax,
    InvalidUtf8,
    NotObject,
    Mixed,
    Method,
    Id,
    ResponseArms,
    ResponseFrame,
    Member,
}

/// Decode one canonical RPC frame. Classification uses a JSON value so mixed
/// request/response members and invalid IDs are rejected before typed decode;
/// the typed decode then catches unknown and duplicate members.
pub(crate) fn decode_frame(text: &str) -> Result<Frame, FrameReject> {
    use serde::Deserialize;
    use serde_json::Value;

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Message {
        #[serde(default)]
        id: Option<u64>,
        method: String,
        #[serde(default)]
        params: Value,
    }

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Error {
        code: RpcErrorCode,
        message: String,
        #[serde(default)]
        data: Value,
    }

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Response {
        id: u64,
        #[serde(default)]
        result: Value,
        #[serde(default)]
        error: Option<Error>,
    }

    let value: Value = serde_json::from_str(text).map_err(|_| FrameReject::Syntax)?;
    let object = value.as_object().ok_or(FrameReject::NotObject)?;
    let has_method = object.contains_key("method");
    let has_result = object.contains_key("result");
    let has_error = object.contains_key("error");

    if has_method && (has_result || has_error) {
        return Err(FrameReject::Mixed);
    }
    if object.keys().any(|key| {
        !matches!(
            key.as_str(),
            "id" | "method" | "params" | "result" | "error"
        )
    }) {
        return Err(FrameReject::Member);
    }

    if has_method {
        let method = object
            .get("method")
            .and_then(Value::as_str)
            .ok_or(FrameReject::Method)?;
        if method.is_empty() || method.len() > MAX_METHOD_BYTES {
            return Err(FrameReject::Method);
        }
        if let Some(id) = object.get("id")
            && id.as_u64().is_none()
        {
            return Err(FrameReject::Id);
        }
        let message: Message = serde_json::from_str(text).map_err(|_| FrameReject::Member)?;
        return Ok(Frame::Message(RpcMessage {
            id: message.id,
            method: message.method,
            params: message.params,
        }));
    }

    if has_result == has_error {
        return Err(FrameReject::ResponseArms);
    }
    if object.get("id").and_then(Value::as_u64).is_none() {
        return Err(FrameReject::Id);
    }
    let response: Response = serde_json::from_str(text).map_err(|_| FrameReject::Member)?;
    if has_result {
        Ok(Frame::Response(RpcResponse::ok(
            response.id,
            response.result,
        )))
    } else {
        let error = response.error.ok_or(FrameReject::ResponseArms)?;
        Ok(Frame::Response(RpcResponse::err(
            response.id,
            RpcError {
                code: error.code,
                message: error.message,
                data: error.data,
            },
        )))
    }
}

/// What a streamed run produced besides the sink's own state.
#[derive(Debug)]
pub struct StreamRun<S> {
    pub sink: S,
    /// Tail of stderr, capped.
    pub stderr: String,
    /// Whether output was cut off by an output limit or the sink.
    pub truncated: bool,
}

/// Collects every message — the classic [`spawn_ndjson`] behaviour.
struct Collect {
    run: PluginRun,
    retained_bytes: usize,
}

impl NdjsonSink for Collect {
    fn line(&mut self, text: &str) -> bool {
        let retained_bytes = match decode_frame(text) {
            Ok(Frame::Message(message)) => {
                self.run.messages.push(message);
                text.len()
            }
            Ok(Frame::Response(_)) => self.keep_junk(text, FrameReject::ResponseFrame),
            Err(reason) => self.keep_junk(text, reason),
        };
        self.retained_bytes = self.retained_bytes.saturating_add(retained_bytes);
        if self.retained_bytes > MAX_RUN_BYTES {
            self.run.truncated = true;
            return false;
        }
        true
    }

    fn invalid_utf8(&mut self, lossy: &str) -> bool {
        let retained_bytes = self.keep_junk(lossy, FrameReject::InvalidUtf8);
        self.retained_bytes = self.retained_bytes.saturating_add(retained_bytes);
        if self.retained_bytes > MAX_RUN_BYTES {
            self.run.truncated = true;
            return false;
        }
        true
    }
}

impl Collect {
    fn keep_junk(&mut self, text: &str, reason: FrameReject) -> usize {
        tracing::debug!(
            target: "thegn::plugin",
            ?reason,
            "discarding rejected plugin frame"
        );
        let kept: String = text.chars().take(200).collect();
        let bytes = kept.len();
        self.run.junk.push(kept);
        bytes
    }
}

/// Run `argv` and collect its NDJSON output.
///
/// The child is put in its own process group so a timeout can take down the
/// whole pipeline, not just the script that spawned it. stdin is closed
/// immediately: a plugin that blocks on input gets EOF rather than hanging
/// until the timeout.
pub fn spawn_ndjson(
    argv: &[String],
    env: &BTreeMap<String, String>,
    cwd: Option<&Path>,
    timeout: Duration,
) -> Result<PluginRun, PluginError> {
    let out = spawn_ndjson_stream(
        argv,
        env,
        cwd,
        timeout,
        Collect {
            run: PluginRun::default(),
            retained_bytes: 0,
        },
    )?;
    let mut run = out.sink.run;
    run.stderr = out.stderr;
    run.truncated |= out.truncated;
    Ok(run)
}

/// Run `argv`, handing each stdout line to `sink` as it arrives.
///
/// Same process-group, timeout, line-size and line-count limits as
/// [`spawn_ndjson`]; the difference is that nothing is retained here.
pub fn spawn_ndjson_stream<S: NdjsonSink>(
    argv: &[String],
    env: &BTreeMap<String, String>,
    cwd: Option<&Path>,
    timeout: Duration,
    sink: S,
) -> Result<StreamRun<S>, PluginError> {
    let Some((program, args)) = argv.split_first() else {
        return Err(PluginError::Spawn("empty command".into()));
    };
    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in env {
        cmd.env(k, v);
    }
    if let Some(d) = cwd.filter(|d| d.is_dir()) {
        cmd.current_dir(d);
    }
    // Inherited git state would make a plugin that shells out to git operate on
    // whatever repo thegn happened to be looking at — the same scrub
    // `agent_run` does.
    for var in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
    ] {
        cmd.env_remove(var);
    }
    set_process_group(&mut cmd);

    let mut child = cmd.spawn().map_err(|e| PluginError::Spawn(e.to_string()))?;
    let pid = child.id();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    // Drain both pipes on their own threads: a child that fills the stderr pipe
    // while we read stdout would otherwise deadlock.
    let (stdout_tx, stdout_rx) = mpsc::channel();
    let (stderr_tx, stderr_rx) = mpsc::channel();
    thegn_core::thread_name::spawn_named("thegn-plug-out", move || {
        let mut sink = sink;
        let mut truncated = false;
        let mut accepting = true;
        let mut lines = 0usize;
        if let Some(out) = stdout {
            let mut buf = BufReader::new(out);
            let mut line = Vec::new();
            let mut oversized = false;
            loop {
                let (consumed, newline, eof) = match buf.fill_buf() {
                    Ok([]) => (0, false, true),
                    Ok(available) => {
                        let end = available
                            .iter()
                            .position(|byte| *byte == b'\n')
                            .map_or(available.len(), |idx| idx + 1);
                        let newline = available.get(end.wrapping_sub(1)) == Some(&b'\n');
                        let remaining = MAX_LINE_BYTES.saturating_sub(line.len());
                        if !oversized {
                            if end <= remaining {
                                line.extend_from_slice(&available[..end]);
                            } else {
                                line.clear();
                                oversized = true;
                                truncated = true;
                            }
                        }
                        (end, newline, false)
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                };
                if eof {
                    if !oversized && !line.is_empty() && accepting && lines < MAX_LINES {
                        deliver_line(&mut sink, &line, &mut accepting, &mut lines);
                        truncated |= !accepting;
                    } else if !line.is_empty() || oversized {
                        truncated = true;
                    }
                    break;
                }
                buf.consume(consumed);
                if !newline {
                    continue;
                }
                if oversized {
                    // Discard the rest of this one frame, then resume at the
                    // next frame boundary. Never reinterpret a suffix as JSON.
                    truncated = true;
                    oversized = false;
                    line.clear();
                    continue;
                }
                if lines >= MAX_LINES {
                    truncated = true;
                    line.clear();
                    continue;
                }
                deliver_line(&mut sink, &line, &mut accepting, &mut lines);
                truncated |= !accepting;
                line.clear();
            }
        }
        if stdout_tx.send((sink, truncated)).is_err() {
            tracing::debug!("plugin stdout reader detached: run already gave up");
        }
    });

    thegn_core::thread_name::spawn_named("thegn-plug-err", move || {
        let mut s = String::new();
        if let Some(e) = stderr {
            // Drain to EOF, keeping only the first MAX_STDERR bytes. Stopping
            // early would SIGPIPE a plugin that logs heavily — same reason as
            // the stdout reader above.
            let mut buf = Vec::new();
            let mut chunk = [0u8; 8192];
            let mut e = e;
            while let Ok(n) = e.read(&mut chunk) {
                if n == 0 {
                    break;
                }
                if buf.len() < MAX_STDERR {
                    let room = MAX_STDERR - buf.len();
                    buf.extend_from_slice(&chunk[..n.min(room)]);
                }
            }
            s = String::from_utf8_lossy(&buf).into_owned();
        }
        if stderr_tx.send(s).is_err() {
            tracing::debug!("plugin stderr reader detached: run already gave up");
        }
    });

    // Observe the leader without reaping it, so its pid still pins the process
    // group when descendants are terminated.
    let deadline = Instant::now() + timeout;
    let mut timed_out = false;
    #[cfg(unix)]
    let status = 'run: loop {
        match leader_exited_nowait(pid) {
            Ok(true) => {
                // A successful one-shot run also kills helpers left in its process group,
                // matching gh_run; backgrounded helpers do not outlive the plugin run.
                kill_group(pid);
                break child.wait().ok();
            }
            Ok(false) => {}
            Err(_) => {
                hand_off_unreaped_child(child);
                break None;
            }
        }
        if Instant::now() >= deadline {
            timed_out = true;
            kill_group(pid);
            let reap_deadline = Instant::now() + READER_GRACE;
            loop {
                match leader_exited_nowait(pid) {
                    Ok(true) => break 'run child.wait().ok(),
                    Ok(false) => {}
                    Err(_) => {
                        hand_off_unreaped_child(child);
                        break 'run None;
                    }
                }
                if Instant::now() >= reap_deadline {
                    hand_off_unreaped_child(child);
                    break 'run None;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    #[cfg(not(unix))]
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {}
            Err(_) => {
                hand_off_unreaped_child(child);
                break None;
            }
        }
        if Instant::now() >= deadline {
            timed_out = true;
            kill_group(pid);
            hand_off_unreaped_child(child);
            break None;
        }
        std::thread::sleep(Duration::from_millis(20));
    };

    let drain_deadline = if timed_out || status.is_none() {
        deadline + READER_GRACE
    } else {
        Instant::now() + READER_GRACE
    };
    let stdout_result =
        recv_by_deadline(&stdout_rx, drain_deadline).map_err(|error| match error {
            mpsc::RecvTimeoutError::Timeout => PluginError::Timeout(timeout.as_secs()),
            mpsc::RecvTimeoutError::Disconnected => {
                PluginError::Protocol("plugin output reader failed".into())
            }
        })?;
    let stderr = recv_by_deadline(&stderr_rx, drain_deadline).map_err(|error| match error {
        mpsc::RecvTimeoutError::Timeout => PluginError::Timeout(timeout.as_secs()),
        mpsc::RecvTimeoutError::Disconnected => {
            PluginError::Protocol("plugin stderr reader failed".into())
        }
    })?;
    let (sink, truncated) = stdout_result;

    match (timed_out, status) {
        (true, _) | (false, None) => Err(PluginError::Timeout(timeout.as_secs())),
        (false, Some(s)) if !s.success() => Err(PluginError::Exit {
            code: s.code(),
            stderr,
        }),
        (false, Some(_)) => Ok(StreamRun {
            sink,
            stderr,
            truncated,
        }),
    }
}

/// Preserve the bounded caller deadline while ensuring every live child is
/// eventually waited on. `try_wait` also handles the case where another owner
/// already reaped the child.
pub(super) fn hand_off_unreaped_child(mut child: std::process::Child) {
    match child.try_wait() {
        Ok(Some(_)) => {}
        Ok(None) | Err(_) => {
            thegn_core::thread_name::spawn_named("thegn-plug-reap", move || {
                if let Err(error) = child.wait() {
                    tracing::debug!(error = %error, "plugin reaper: wait failed");
                }
            });
        }
    }
}

fn deliver_line<S: NdjsonSink>(sink: &mut S, line: &[u8], accepting: &mut bool, lines: &mut usize) {
    if !*accepting || *lines >= MAX_LINES {
        return;
    }
    let Ok(valid) = std::str::from_utf8(line) else {
        let lossy = String::from_utf8_lossy(line);
        let text = lossy.trim();
        if text.is_empty() {
            return;
        }
        *lines += 1;
        *accepting = sink.invalid_utf8(text);
        return;
    };
    let text = valid.trim();
    if text.is_empty() {
        return;
    }
    *lines += 1;
    *accepting = sink.line(text);
}

fn recv_by_deadline<T>(
    receiver: &mpsc::Receiver<T>,
    deadline: Instant,
) -> Result<T, mpsc::RecvTimeoutError> {
    receiver.recv_timeout(deadline.saturating_duration_since(Instant::now()))
}

#[cfg(unix)]
pub(crate) fn set_process_group(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;
    // 0 = become the leader of a new group, so `killpg` below reaches every
    // descendant rather than just the script we started.
    cmd.process_group(0);
}

#[cfg(not(unix))]
pub(crate) fn set_process_group(_cmd: &mut Command) {}

/// Whether this platform has direct group termination without launching an
/// unbounded helper. Callers promising a deadline must refuse before spawn
/// when this is false; the legacy taskkill path does not provide that contract.
pub(crate) const fn bounded_group_termination_supported() -> bool {
    cfg!(unix)
}

/// Whether the (direct, still-unreaped) child `pid` has exited, observed with
/// `WNOWAIT` so it stays waitable: its zombie pins the pid/pgid, so a group
/// kill issued before the reap can never hit a recycled, unrelated group.
#[cfg(unix)]
pub(crate) fn leader_exited_nowait(pid: u32) -> std::io::Result<bool> {
    let mut info = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
    // SAFETY: waitid initializes siginfo on success; WNOWAIT leaves the child
    // waitable.
    let rc = unsafe {
        libc::waitid(
            libc::P_PID,
            pid as libc::id_t,
            info.as_mut_ptr(),
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: successful waitid initialized the siginfo structure.
    Ok(unsafe { info.assume_init().si_pid() } != 0)
}

#[cfg(not(unix))]
pub(crate) fn leader_exited_nowait(_pid: u32) -> std::io::Result<bool> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "no-wait child observation needs Unix",
    ))
}

/// Block until the leader has exited, WITHOUT reaping it (`WNOWAIT`): the
/// zombie pins the pid/pgid, so group signals sent before the reap cannot hit a
/// recycled, unrelated group. Retries on `EINTR`.
#[cfg(unix)]
pub(crate) fn wait_leader_exit_nowait(pid: u32) -> std::io::Result<()> {
    loop {
        let mut info = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
        // SAFETY: waitid initializes siginfo on success; WNOWAIT leaves the
        // child waitable.
        let rc = unsafe {
            libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                info.as_mut_ptr(),
                libc::WEXITED | libc::WNOWAIT,
            )
        };
        if rc == 0 {
            return Ok(());
        }
        let err = std::io::Error::last_os_error();
        if err.kind() != std::io::ErrorKind::Interrupted {
            return Err(err);
        }
    }
}

#[cfg(not(unix))]
pub(crate) fn wait_leader_exit_nowait(_pid: u32) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "no-wait child observation needs Unix",
    ))
}

/// Ask the whole group to terminate (`SIGTERM`). Only call while the leader is
/// still unreaped.
#[cfg(unix)]
pub(crate) fn term_group(pid: u32) {
    // SAFETY: plain signal syscall; negative pid targets the group.
    unsafe {
        libc::kill(-(pid as i32), libc::SIGTERM);
    }
}

#[cfg(not(unix))]
pub(crate) fn term_group(pid: u32) {
    kill_group(pid);
}

#[cfg(unix)]
pub(crate) fn kill_group(pid: u32) {
    // Negative pid targets the whole group.
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
}

#[cfg(not(unix))]
pub(crate) fn kill_group(pid: u32) {
    // No process groups here; `taskkill /T` walks the tree instead.
    let _ = Command::new("taskkill") // best-effort: kill failure surfaces via the next poll of the child
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(test)]
#[path = "proc_tests.rs"]
mod tests;
