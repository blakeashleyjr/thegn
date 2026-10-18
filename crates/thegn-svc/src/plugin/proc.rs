//! Run a program and read newline-delimited JSON back from it.
//!
//! Every limit here exists because the program is user-supplied and may be
//! badly behaved: a runaway plugin must not be able to OOM the compositor, pin
//! a CPU, or outlive the fetch that started it.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::{Command, Stdio};
#[cfg(unix)]
use std::sync::Arc;
#[cfg(unix)]
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

#[cfg(not(unix))]
use std::io::{BufRead, BufReader, Read};

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
/// Maximum nesting depth of an entire plugin RPC frame, including its envelope.
pub(crate) const MAX_FRAME_DEPTH: usize = 32;
/// Maximum time to wait for a killed leader and its pipe readers to finish.
const READER_GRACE: Duration = Duration::from_millis(500);
/// Let the caller receive the result after the reader reaches its cutoff.
const READER_RECEIVE_MARGIN: Duration = Duration::from_millis(100);

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

fn timeout_error(secs: u64, outcome: RunOutcome) -> PluginError {
    #[cfg(test)]
    LAST_TIMEOUT_OUTCOME.with(|last| last.set(Some(outcome)));
    #[cfg(not(test))]
    tracing::debug!(target: "thegn::plugin", ?outcome, "plugin timeout process-tree outcome");
    PluginError::Timeout(secs)
}

#[cfg(test)]
thread_local! {
    static LAST_TIMEOUT_OUTCOME: std::cell::Cell<Option<RunOutcome>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
fn last_timeout_outcome() -> Option<RunOutcome> {
    LAST_TIMEOUT_OUTCOME.with(std::cell::Cell::get)
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
    /// Process and pipe cleanup observed for this run.
    pub outcome: RunOutcome,
}

/// Bounded process-tree cleanup result for a one-shot plugin run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RunOutcome {
    /// A process-group termination was requested.
    pub killed: bool,
    /// The direct child was reaped by this run.
    pub reaped: bool,
    /// Both output pipes reached EOF before the drain cutoff.
    pub drained: bool,
    /// A platform reader was abandoned because an inherited pipe stayed open.
    pub abandoned: bool,
    /// Poll calls made by the Unix pipe reader for this run (test diagnostics).
    #[cfg(test)]
    pub poll_calls: usize,
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
    Depth,
}

/// Decode one canonical RPC frame. One duplicate-aware JSON parse supports
/// classification and typed construction without losing duplicate members.
pub(crate) fn decode_frame(text: &str) -> Result<Frame, FrameReject> {
    use serde::de::{self, MapAccess, SeqAccess, Visitor};
    use serde::{Deserialize, Deserializer};
    use serde_json::Value;
    use std::fmt;

    struct UniqueValue(Value);
    const DUPLICATE_MEMBER_ERROR: &str = "__thegn_duplicate_json_member__";

    impl<'de> Deserialize<'de> for UniqueValue {
        fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
        where
            D: Deserializer<'de>,
        {
            struct UniqueValueVisitor;

            impl<'de> Visitor<'de> for UniqueValueVisitor {
                type Value = UniqueValue;

                fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                    formatter.write_str("a JSON value without duplicate object members")
                }

                fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
                    Ok(UniqueValue(Value::Bool(value)))
                }

                fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
                    Ok(UniqueValue(Value::Number(value.into())))
                }

                fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
                    Ok(UniqueValue(Value::Number(value.into())))
                }

                fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
                where
                    E: de::Error,
                {
                    let number = serde_json::Number::from_f64(value)
                        .ok_or_else(|| E::custom("invalid JSON number"))?;
                    Ok(UniqueValue(Value::Number(number)))
                }

                fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
                    Ok(UniqueValue(Value::String(value.to_owned())))
                }

                fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
                    Ok(UniqueValue(Value::String(value)))
                }

                fn visit_unit<E>(self) -> Result<Self::Value, E> {
                    Ok(UniqueValue(Value::Null))
                }

                fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
                where
                    A: SeqAccess<'de>,
                {
                    let mut values = Vec::new();
                    while let Some(UniqueValue(value)) = sequence.next_element()? {
                        values.push(value);
                    }
                    Ok(UniqueValue(Value::Array(values)))
                }

                fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
                where
                    A: MapAccess<'de>,
                {
                    let mut values = serde_json::Map::new();
                    while let Some(key) = map.next_key::<String>()? {
                        if values.contains_key(&key) {
                            return Err(de::Error::custom(DUPLICATE_MEMBER_ERROR));
                        }
                        let UniqueValue(value) = map.next_value()?;
                        values.insert(key, value);
                    }
                    Ok(UniqueValue(Value::Object(values)))
                }
            }

            deserializer.deserialize_any(UniqueValueVisitor)
        }
    }

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

    if json_depth_exceeds(text, MAX_FRAME_DEPTH) {
        return Err(FrameReject::Depth);
    }
    let UniqueValue(value) = serde_json::from_str(text).map_err(|error| {
        if error.to_string().contains(DUPLICATE_MEMBER_ERROR) {
            FrameReject::Member
        } else {
            FrameReject::Syntax
        }
    })?;
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
        let message: Message = serde_json::from_value(value).map_err(|_| FrameReject::Member)?;
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
    let response: Response = serde_json::from_value(value).map_err(|_| FrameReject::Member)?;
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

/// Return whether JSON container nesting exceeds `limit`, without allocating.
/// Brackets in strings are ignored, including those following escaped quotes.
pub(crate) fn json_depth_exceeds(text: &str, limit: usize) -> bool {
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;

    for byte in text.bytes() {
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            continue;
        }

        match byte {
            b'"' => in_string = true,
            b'[' | b'{' => {
                depth = depth.saturating_add(1);
                if depth > limit {
                    return true;
                }
            }
            b']' | b'}' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }

    false
}

/// What a streamed run produced besides the sink's own state.
#[derive(Debug)]
pub struct StreamRun<S> {
    pub sink: S,
    /// Tail of stderr, capped.
    pub stderr: String,
    /// Whether output was cut off by an output limit or the sink.
    pub truncated: bool,
    /// Process and pipe cleanup observed for this run.
    pub outcome: RunOutcome,
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
    run.outcome = out.outcome;
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

    let started = Instant::now();
    let deadline = started + timeout;
    #[cfg(unix)]
    let reader_deadline_ms = Arc::new(AtomicU64::new(
        timeout
            .saturating_add(READER_GRACE)
            .as_millis()
            .min(u64::MAX as u128) as u64,
    ));
    #[cfg(unix)]
    let (pipe_tx, pipe_rx) = mpsc::channel();
    #[cfg(unix)]
    let reader_deadline = Arc::clone(&reader_deadline_ms);
    #[cfg(unix)]
    let pipe_thread = thegn_core::thread_name::spawn_named("thegn-plug-pipe", move || {
        read_plugin_pipes(stdout, stderr, sink, started, reader_deadline, pipe_tx);
    });

    // Windows keeps separate blocking readers. A descendant can retain an
    // inherited pipe there; the receiver deadline reports abandonment, while
    // the detached reader thread can remain blocked until that handle closes.
    #[cfg(not(unix))]
    let (stdout_tx, stdout_rx) = mpsc::channel();
    #[cfg(not(unix))]
    let (stderr_tx, stderr_rx) = mpsc::channel();
    #[cfg(not(unix))]
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

    #[cfg(not(unix))]
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
    let mut timed_out = false;
    let mut kill_requested = false;
    #[cfg(unix)]
    let status = 'run: loop {
        match leader_exited_nowait(pid) {
            Ok(true) => {
                // A successful one-shot run also kills helpers left in its process group,
                // matching gh_run; backgrounded helpers do not outlive the plugin run.
                kill_requested = true;
                #[cfg(unix)]
                reader_deadline_ms.store(
                    started
                        .elapsed()
                        .saturating_add(READER_GRACE)
                        .as_millis()
                        .min(u64::MAX as u128) as u64,
                    Ordering::Release,
                );
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
            kill_requested = true;
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
            kill_requested = true;
            kill_group(pid);
            hand_off_unreaped_child(child);
            break None;
        }
        std::thread::sleep(Duration::from_millis(20));
    };

    #[cfg(unix)]
    let (stdout_result, stderr, drained, abandoned) = match recv_by_deadline(
        &pipe_rx,
        started
            + Duration::from_millis(reader_deadline_ms.load(Ordering::Acquire))
            + READER_RECEIVE_MARGIN,
    ) {
        Ok(Ok((sink, stderr, truncated, drained, abandoned, poll_calls))) => {
            match pipe_thread.join() {
                Ok(()) => {}
                Err(_) => {
                    tracing::warn!(target: "thegn::plugin", program, "plugin pipe reader panicked")
                }
            }
            ((sink, truncated, poll_calls), stderr, drained, abandoned)
        }
        Ok(Err(error)) => return Err(PluginError::Protocol(error)),
        Err(error) => {
            let outcome = RunOutcome {
                killed: kill_requested,
                reaped: status.is_some(),
                drained: false,
                abandoned: true,
                #[cfg(test)]
                poll_calls: 0,
            };
            tracing::debug!(target: "thegn::plugin", program, ?outcome, "plugin pipe reader exceeded drain deadline");
            return match error {
                mpsc::RecvTimeoutError::Timeout => Err(timeout_error(timeout.as_secs(), outcome)),
                mpsc::RecvTimeoutError::Disconnected => {
                    Err(PluginError::Protocol("plugin pipe reader failed".into()))
                }
            };
        }
    };
    #[cfg(not(unix))]
    let drain_deadline = if status.is_some() && !timed_out {
        Instant::now() + READER_GRACE
    } else {
        deadline + READER_GRACE
    };
    #[cfg(not(unix))]
    let (stdout_result, stderr, drained, abandoned) = {
        let stdout_result = match recv_by_deadline(
            &stdout_rx,
            drain_deadline + READER_RECEIVE_MARGIN,
        ) {
            Ok(result) => result,
            Err(error) => {
                let outcome = RunOutcome {
                    killed: kill_requested,
                    reaped: status.is_some(),
                    drained: false,
                    abandoned: true,
                    #[cfg(test)]
                    poll_calls: 0,
                };
                tracing::debug!(target: "thegn::plugin", program, ?outcome, "Windows plugin stdout reader abandoned");
                return match error {
                    mpsc::RecvTimeoutError::Timeout => {
                        Err(timeout_error(timeout.as_secs(), outcome))
                    }
                    mpsc::RecvTimeoutError::Disconnected => {
                        Err(PluginError::Protocol("plugin output reader failed".into()))
                    }
                };
            }
        };
        let stderr = match recv_by_deadline(&stderr_rx, drain_deadline + READER_RECEIVE_MARGIN) {
            Ok(stderr) => stderr,
            Err(error) => {
                let outcome = RunOutcome {
                    killed: kill_requested,
                    reaped: status.is_some(),
                    drained: false,
                    abandoned: true,
                    #[cfg(test)]
                    poll_calls: 0,
                };
                tracing::debug!(target: "thegn::plugin", program, ?outcome, "Windows plugin stderr reader abandoned");
                return match error {
                    mpsc::RecvTimeoutError::Timeout => {
                        Err(timeout_error(timeout.as_secs(), outcome))
                    }
                    mpsc::RecvTimeoutError::Disconnected => {
                        Err(PluginError::Protocol("plugin stderr reader failed".into()))
                    }
                };
            }
        };
        let (sink, truncated) = stdout_result;
        ((sink, truncated, 0), stderr, true, false)
    };
    let (sink, truncated, _poll_calls) = stdout_result;
    let outcome = RunOutcome {
        killed: kill_requested,
        reaped: status.is_some(),
        drained,
        abandoned,
        #[cfg(test)]
        poll_calls: _poll_calls,
    };
    tracing::debug!(target: "thegn::plugin", program, ?outcome, "plugin run process-tree outcome");

    match (timed_out, status) {
        (true, _) | (false, None) => Err(timeout_error(timeout.as_secs(), outcome)),
        (false, Some(s)) if !s.success() => Err(PluginError::Exit {
            code: s.code(),
            stderr,
        }),
        (false, Some(_)) => Ok(StreamRun {
            sink,
            stderr,
            truncated,
            outcome,
        }),
    }
}

/// Preserve the bounded caller deadline while ensuring every live child is
/// eventually waited on. `try_wait` also handles the case where another owner
/// already reaped the child. The reaper may wait indefinitely if the OS does
/// not deliver the requested termination; the run outcome records `reaped: false`.
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

/// One pipe-reader result: the sink, captured stderr, the truncated, drained,
/// abandoned and poll-count values, or the reader's error text.
#[cfg(unix)]
type PipeReadResult<S> = Result<(S, String, bool, bool, bool, usize), String>;

#[cfg(unix)]
fn read_plugin_pipes<S: NdjsonSink>(
    stdout: Option<std::process::ChildStdout>,
    stderr: Option<std::process::ChildStderr>,
    sink: S,
    started: Instant,
    deadline_ms: Arc<AtomicU64>,
    sender: mpsc::Sender<PipeReadResult<S>>,
) {
    #[cfg(test)]
    let _reader_guard = LivePipeReaderGuard::new();
    use std::os::fd::AsRawFd;

    // Both streams are configured as piped above. Keep this failure explicit
    // if that setup changes, rather than silently disconnecting the receiver.
    let Some(stdout) = stdout else {
        if let Err(error) = sender.send(Err("stdout pipe was not captured".into())) {
            tracing::debug!(%error, "plugin pipe reader result was not received");
        }
        return;
    };
    let Some(stderr) = stderr else {
        if let Err(error) = sender.send(Err("stderr pipe was not captured".into())) {
            tracing::debug!(%error, "plugin pipe reader result was not received");
        }
        return;
    };
    let stdout_fd = stdout.as_raw_fd();
    let stderr_fd = stderr.as_raw_fd();
    if set_nonblocking(stdout_fd).is_err() || set_nonblocking(stderr_fd).is_err() {
        if let Err(error) =
            sender.send(Err("could not configure plugin pipes as nonblocking".into()))
        {
            tracing::debug!(%error, "plugin pipe reader result was not received");
        }
        return;
    }

    let mut framer = OutputFramer::new(sink);
    let mut stderr_bytes = Vec::with_capacity(MAX_STDERR);
    let mut stdout_open = true;
    let mut stderr_open = true;
    let mut stdout_eof = false;
    let mut stderr_eof = false;
    let mut chunk = [0_u8; 8192];
    let mut descriptors = [
        libc::pollfd {
            fd: stdout_fd,
            events: libc::POLLIN,
            revents: 0,
        },
        libc::pollfd {
            fd: stderr_fd,
            events: libc::POLLIN,
            revents: 0,
        },
    ];
    let mut poll_calls = 0usize;
    while stdout_open || stderr_open {
        let deadline = started + Duration::from_millis(deadline_ms.load(Ordering::Acquire));
        if Instant::now() >= deadline {
            break;
        }
        // Keep closed streams out of poll: a pipe at EOF reports POLLHUP
        // continuously, which would spin while its sibling remains open.
        descriptors[0].fd = if stdout_open { stdout_fd } else { -1 };
        descriptors[0].events = if stdout_open { libc::POLLIN } else { 0 };
        descriptors[0].revents = 0;
        descriptors[1].fd = if stderr_open { stderr_fd } else { -1 };
        descriptors[1].events = if stderr_open { libc::POLLIN } else { 0 };
        descriptors[1].revents = 0;
        poll_calls = poll_calls.saturating_add(1);
        let remaining = deadline.saturating_duration_since(Instant::now());
        let wait_ms = poll_wait_ms(remaining);
        // SAFETY: descriptors points to two initialized pollfd entries.
        let polled =
            unsafe { libc::poll(descriptors.as_mut_ptr(), descriptors.len() as _, wait_ms) };
        if polled < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            break;
        }
        if polled == 0 {
            continue;
        }
        for (index, fd) in [stdout_fd, stderr_fd].into_iter().enumerate() {
            let events = descriptors[index].revents;
            if events == 0 {
                continue;
            }
            let mut open = if index == 0 { stdout_open } else { stderr_open };
            if events & (libc::POLLERR | libc::POLLNVAL) != 0 {
                open = false;
            } else {
                while Instant::now()
                    < started + Duration::from_millis(deadline_ms.load(Ordering::Acquire))
                {
                    // SAFETY: fd is owned by this thread, chunk is writable, and
                    // the descriptor is nonblocking.
                    let count = unsafe { libc::read(fd, chunk.as_mut_ptr().cast(), chunk.len()) };
                    if count == 0 {
                        open = false;
                        if index == 0 {
                            stdout_eof = true;
                        } else {
                            stderr_eof = true;
                        }
                        break;
                    }
                    if count < 0 {
                        let error = std::io::Error::last_os_error();
                        if error.kind() == std::io::ErrorKind::Interrupted {
                            continue;
                        }
                        if error.kind() == std::io::ErrorKind::WouldBlock {
                            break;
                        }
                        open = false;
                        break;
                    }
                    let bytes = &chunk[..count as usize];
                    if index == 0 {
                        framer.push(bytes);
                    } else if stderr_bytes.len() < MAX_STDERR {
                        let take = bytes.len().min(MAX_STDERR - stderr_bytes.len());
                        stderr_bytes.extend_from_slice(&bytes[..take]);
                    }
                }
            }
            if index == 0 {
                stdout_open = open;
                if !open {
                    descriptors[index].fd = -1;
                    descriptors[index].events = 0;
                }
            } else {
                stderr_open = open;
                if !open {
                    descriptors[index].fd = -1;
                    descriptors[index].events = 0;
                }
            }
            if !stdout_open && !stderr_open {
                break;
            }
        }
    }
    let drained = stdout_eof && stderr_eof;
    let deadline = started + Duration::from_millis(deadline_ms.load(Ordering::Acquire));
    let abandoned = !drained && Instant::now() >= deadline && (stdout_open || stderr_open);
    let (sink, truncated) = framer.finish(stdout_eof);
    let stderr = String::from_utf8_lossy(&stderr_bytes).into_owned();
    if let Err(error) = sender.send(Ok((
        sink, stderr, truncated, drained, abandoned, poll_calls,
    ))) {
        tracing::debug!(%error, "plugin pipe reader result was not received");
    }
}

#[cfg(unix)]
fn poll_wait_ms(remaining: Duration) -> i32 {
    if remaining.is_zero() {
        0
    } else {
        remaining.as_micros().div_ceil(1_000).clamp(1, 50) as i32
    }
}

#[cfg(all(test, unix))]
static LIVE_PIPE_READERS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

#[cfg(all(test, unix))]
struct LivePipeReaderGuard;

#[cfg(all(test, unix))]
impl LivePipeReaderGuard {
    fn new() -> Self {
        LIVE_PIPE_READERS.fetch_add(1, Ordering::SeqCst);
        Self
    }
}

#[cfg(all(test, unix))]
impl Drop for LivePipeReaderGuard {
    fn drop(&mut self) {
        LIVE_PIPE_READERS.fetch_sub(1, Ordering::SeqCst);
    }
}

#[cfg(unix)]
fn set_nonblocking(fd: std::os::fd::RawFd) -> std::io::Result<()> {
    // SAFETY: fcntl only reads the descriptor flags for this open fd.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: fcntl updates flags on the valid owned fd.
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(unix)]
struct OutputFramer<S> {
    sink: S,
    truncated: bool,
    accepting: bool,
    lines: usize,
    line: Vec<u8>,
    oversized: bool,
}

#[cfg(unix)]
impl<S: NdjsonSink> OutputFramer<S> {
    fn new(sink: S) -> Self {
        Self {
            sink,
            truncated: false,
            accepting: true,
            lines: 0,
            line: Vec::with_capacity(MAX_LINE_BYTES.min(8192)),
            oversized: false,
        }
    }

    fn push(&mut self, mut bytes: &[u8]) {
        while !bytes.is_empty() {
            let end = bytes
                .iter()
                .position(|byte| *byte == b'\n')
                .map_or(bytes.len(), |i| i + 1);
            let newline = bytes[end - 1] == b'\n';
            let remaining = MAX_LINE_BYTES.saturating_sub(self.line.len());
            if !self.oversized {
                if end <= remaining {
                    self.line.extend_from_slice(&bytes[..end]);
                } else {
                    self.line.clear();
                    self.oversized = true;
                    self.truncated = true;
                }
            }
            bytes = &bytes[end..];
            if newline {
                if self.oversized {
                    self.truncated = true;
                    self.oversized = false;
                    self.line.clear();
                } else if self.lines >= MAX_LINES {
                    self.truncated = true;
                    self.line.clear();
                } else {
                    deliver_line(
                        &mut self.sink,
                        &self.line,
                        &mut self.accepting,
                        &mut self.lines,
                    );
                    self.truncated |= !self.accepting;
                    self.line.clear();
                }
            }
        }
    }

    fn finish(mut self, eof: bool) -> (S, bool) {
        if eof
            && !self.oversized
            && !self.line.is_empty()
            && self.accepting
            && self.lines < MAX_LINES
        {
            deliver_line(
                &mut self.sink,
                &self.line,
                &mut self.accepting,
                &mut self.lines,
            );
            self.truncated |= !self.accepting;
        } else if !self.line.is_empty() || self.oversized {
            self.truncated = true;
        }
        (self.sink, self.truncated)
    }
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
    if unsafe { libc::kill(-(pid as i32), libc::SIGKILL) } == -1 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error().is_none_or(kill_errno_should_warn) {
            tracing::warn!(pid, %error, "failed to kill plugin process group");
        }
    }
}

#[cfg(unix)]
fn kill_errno_should_warn(errno: i32) -> bool {
    errno != libc::ESRCH
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

#[cfg(all(test, unix))]
mod process_tree_deadline_tests {
    use super::{
        LIVE_PIPE_READERS, PluginError, READER_GRACE, READER_RECEIVE_MARGIN, poll_wait_ms,
        spawn_ndjson,
    };
    use std::collections::BTreeMap;
    use std::process::Command;
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    fn setsid_available() -> bool {
        Command::new("setsid")
            .arg("true")
            .status()
            .is_ok_and(|status| status.success())
    }

    fn assert_readers_exit_within_margin() {
        let until = Instant::now() + READER_GRACE + READER_RECEIVE_MARGIN;
        while LIVE_PIPE_READERS.load(Ordering::SeqCst) != 0 && Instant::now() < until {
            std::thread::yield_now();
        }
        assert_eq!(LIVE_PIPE_READERS.load(Ordering::SeqCst), 0);
    }

    fn run_holding_pipe(which: &str) -> super::PluginRun {
        let redirection = match which {
            "stdout" => "2>/dev/null",
            "stderr" => ">/dev/null",
            _ => unreachable!(),
        };
        let script =
            format!("setsid sh -c 'sleep 30 & p=$!; sleep 2; kill $p' {redirection} & sleep 0.1");
        let started = Instant::now();
        let run = spawn_ndjson(
            &["sh".into(), "-c".into(), script],
            &BTreeMap::new(),
            None,
            Duration::from_millis(300),
        )
        .unwrap();
        assert!(started.elapsed() < Duration::from_secs(4));
        run
    }

    #[test]
    fn setsid_descendant_holding_stdout_cannot_outlive_pipe_deadline() {
        if !setsid_available() {
            return;
        }
        let run = run_holding_pipe("stdout");
        assert!(run.outcome.abandoned);
        assert!(!run.outcome.drained);
        assert!(run.outcome.reaped);
        assert!(!run.truncated);
        assert_readers_exit_within_margin();
    }

    #[test]
    fn setsid_descendant_holding_stderr_cannot_outlive_pipe_deadline() {
        if !setsid_available() {
            return;
        }
        let run = run_holding_pipe("stderr");
        assert!(run.outcome.abandoned);
        assert!(!run.outcome.drained);
        assert!(run.outcome.reaped);
        assert!(!run.truncated);
        // A 300 ms deadline permits at most 300 one-ms waits even if every
        // wait wakes spuriously; 200 is generous for this held pipe, which
        // normally wakes only at the 50 ms cap.
        assert!(
            run.outcome.poll_calls < 200,
            "poll calls: {}",
            run.outcome.poll_calls
        );
        assert_readers_exit_within_margin();
    }

    #[test]
    fn poll_wait_rounds_up_submillisecond_time() {
        assert_eq!(poll_wait_ms(Duration::ZERO), 0);
        assert_eq!(poll_wait_ms(Duration::from_micros(1)), 1);
        assert_eq!(poll_wait_ms(Duration::from_micros(999)), 1);
        assert_eq!(poll_wait_ms(Duration::from_millis(1)), 1);
        assert_eq!(poll_wait_ms(Duration::from_micros(1_500)), 2);
        assert_eq!(poll_wait_ms(Duration::from_millis(120)), 50);
    }

    #[test]
    fn timeout_error_remains_a_timeout_after_bounded_tree_cleanup() {
        if !setsid_available() {
            return;
        }
        let started = Instant::now();
        let result = spawn_ndjson(
            &[
                "sh".into(),
                "-c".into(),
                "setsid sleep 30 & sleep 30".into(),
            ],
            &BTreeMap::new(),
            None,
            Duration::from_millis(300),
        );
        match result {
            Err(PluginError::Timeout(_)) => {
                let outcome = super::last_timeout_outcome().expect("timeout outcome recorded");
                assert!(outcome.killed);
                assert!(!outcome.drained);
                assert!(outcome.reaped);
            }
            other => panic!("expected timeout, got {other:?}"),
        }
        assert!(started.elapsed() < Duration::from_secs(4));
        assert_readers_exit_within_margin();
    }

    #[test]
    fn successful_leader_with_escaped_pipe_holder_returns_after_reader_grace() {
        if !setsid_available() {
            return;
        }
        let started = Instant::now();
        let run = spawn_ndjson(
            &["sh".into(), "-c".into(), "setsid sleep 30 & exit 0".into()],
            &BTreeMap::new(),
            None,
            Duration::from_secs(10),
        )
        .unwrap();
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(run.outcome.abandoned);
        assert!(!run.truncated);
        assert_readers_exit_within_margin();
    }
}

#[cfg(test)]
#[path = "proc_tests.rs"]
mod tests;

#[cfg(all(test, unix))]
mod kill_errno_tests {
    use super::kill_errno_should_warn;

    #[test]
    fn kill_group_errno_classification_silences_esrch_and_warns_other_errors() {
        assert!(!kill_errno_should_warn(libc::ESRCH));
        assert!(kill_errno_should_warn(libc::EPERM));
    }
}
