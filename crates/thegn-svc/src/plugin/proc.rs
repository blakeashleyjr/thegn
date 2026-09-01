//! Run a program and read newline-delimited JSON back from it.
//!
//! Every limit here exists because the program is user-supplied and may be
//! badly behaved: a runaway plugin must not be able to OOM the compositor, pin
//! a CPU, or outlive the fetch that started it.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use thegn_core::plugin_api::RpcMessage;

/// Longest single line accepted. A plugin that writes more than this is
/// malfunctioning, and buffering it would be the OOM.
pub const MAX_LINE_BYTES: usize = 1 << 20;
/// Most messages accepted from one run.
pub const MAX_LINES: usize = 20_000;
/// Bytes of stderr kept for the error message.
pub const MAX_STDERR: usize = 8 << 10;

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
    /// Whether output was cut off by [`MAX_LINES`].
    pub truncated: bool,
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
    let reader = std::thread::spawn(move || {
        let mut run = PluginRun::default();
        if let Some(out) = stdout {
            let mut buf = BufReader::new(out);
            let mut line = Vec::new();
            loop {
                line.clear();
                // read_until, not read_line, so invalid UTF-8 is a skipped line
                // rather than an error that ends the stream.
                let n = match buf
                    .by_ref()
                    .take(MAX_LINE_BYTES as u64)
                    .read_until(b'\n', &mut line)
                {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                // Past the cap we KEEP READING and discard. Closing the pipe
                // instead would hand the plugin a SIGPIPE mid-write, turning an
                // over-chatty plugin into a "killed by signal" failure with no
                // usable output at all — when what we want is its first N
                // messages plus an honest `truncated`.
                if run.truncated || run.messages.len() + run.junk.len() >= MAX_LINES {
                    run.truncated = true;
                    continue;
                }
                let text = String::from_utf8_lossy(&line[..n]).trim().to_string();
                if text.is_empty() {
                    continue;
                }
                match serde_json::from_str::<RpcMessage>(&text) {
                    Ok(m) => run.messages.push(m),
                    Err(_) => run.junk.push(text.chars().take(200).collect()),
                }
            }
        }
        run
    });

    let err_thread = std::thread::spawn(move || {
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
        s
    });

    // Poll for exit rather than `wait()`, so the timeout is enforceable.
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) => {}
            Err(_) => break None,
        }
        if Instant::now() >= deadline {
            kill_group(pid);
            let _ = child.wait(); // best-effort: reap-or-not is terminal here
            break None;
        }
        std::thread::sleep(Duration::from_millis(20));
    };

    let mut run = reader.join().unwrap_or_default();
    run.stderr = err_thread.join().unwrap_or_default();

    match status {
        None => Err(PluginError::Timeout(timeout.as_secs())),
        Some(s) if !s.success() => Err(PluginError::Exit {
            code: s.code(),
            stderr: run.stderr,
        }),
        Some(_) => Ok(run),
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
