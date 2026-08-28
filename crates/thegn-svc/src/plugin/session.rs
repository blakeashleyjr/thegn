//! A resident plugin session: one long-lived child spoken to over NDJSON.
//!
//! [`spawn_ndjson`](super::proc::spawn_ndjson) runs a plugin to completion;
//! this keeps one alive for the whole thegn session. Reads happen on a
//! dedicated thread that hands every parsed line to a callback (the host
//! tags it with the plugin id, queues it on the loop channel and pulses the
//! waker); writes go through a cloneable [`SessionWriter`] so replies can be
//! sent from any thread (the host.call dispatcher answers directly, never
//! touching the event loop).

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex};

use thegn_core::plugin_api::{PluginCallback, RpcMessage, RpcResponse};

use super::proc::{MAX_LINE_BYTES, PluginError};

/// One parsed line (or lifecycle event) from a resident plugin.
#[derive(Debug, Clone)]
pub enum SessionEvent {
    /// A verb or notification from the plugin.
    Message(RpcMessage),
    /// The plugin's reply to a host-initiated request.
    Response(RpcResponse),
    /// A line that was not valid JSON — kept for diagnostics (`println!`
    /// debugging is the most common plugin-author mistake).
    Junk(String),
    /// The process exited; the session is dead. Sent exactly once, last.
    Exit { code: Option<i32> },
}

/// Cloneable stdin handle: `None` after the session dies or is killed.
#[derive(Clone)]
pub struct SessionWriter(Arc<Mutex<Option<ChildStdin>>>);

impl SessionWriter {
    fn write_line(&self, line: &str) -> Result<(), PluginError> {
        let mut guard = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let Some(stdin) = guard.as_mut() else {
            return Err(PluginError::Protocol("session is closed".into()));
        };
        stdin
            .write_all(line.as_bytes())
            .and_then(|()| stdin.write_all(b"\n"))
            .and_then(|()| stdin.flush())
            .map_err(|e| PluginError::Protocol(format!("write failed: {e}")))
    }

    /// Send a host→plugin callback notification (`activate`, `render`,
    /// `on_event`, `deactivate`).
    pub fn notify(
        &self,
        callback: PluginCallback,
        params: serde_json::Value,
    ) -> Result<(), PluginError> {
        let msg = RpcMessage::notification(callback, params);
        self.write_line(&serde_json::to_string(&msg).unwrap_or_default())
    }

    /// Write one raw NDJSON line (the provider bridge's `provider.call`
    /// requests, which carry their own correlation ids).
    pub fn send_raw(&self, line: &str) -> Result<(), PluginError> {
        self.write_line(line)
    }

    /// Answer one of the plugin's `id`-bearing requests.
    pub fn respond(&self, resp: &RpcResponse) -> Result<(), PluginError> {
        self.write_line(&serde_json::to_string(resp).unwrap_or_default())
    }

    /// Drop the stdin handle (EOF to the plugin) — the polite half of
    /// shutdown; `ResidentSession::kill` is the impolite half.
    pub fn close(&self) {
        let mut guard = self.0.lock().unwrap_or_else(|e| e.into_inner());
        *guard = None;
    }
}

/// A live resident plugin process.
pub struct ResidentSession {
    child: Arc<Mutex<Child>>,
    writer: SessionWriter,
}

impl ResidentSession {
    /// Spawn `argv` and start the reader thread. Every stdout line (and the
    /// final exit) is delivered to `on_event`; stderr is drained and logged.
    /// The environment is scrubbed of inherited git state exactly like
    /// [`spawn_ndjson`](super::proc::spawn_ndjson).
    pub fn spawn(
        argv: &[String],
        env: &BTreeMap<String, String>,
        cwd: Option<&Path>,
        on_event: impl Fn(SessionEvent) + Send + 'static,
    ) -> Result<Self, PluginError> {
        let Some((program, args)) = argv.split_first() else {
            return Err(PluginError::Spawn("empty command".into()));
        };
        let mut cmd = Command::new(program);
        cmd.args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (k, v) in env {
            cmd.env(k, v);
        }
        if let Some(d) = cwd.filter(|d| d.is_dir()) {
            cmd.current_dir(d);
        }
        for var in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_INDEX_FILE",
            "GIT_OBJECT_DIRECTORY",
        ] {
            cmd.env_remove(var);
        }
        super::proc::set_process_group(&mut cmd);

        let mut child = cmd.spawn().map_err(|e| PluginError::Spawn(e.to_string()))?;
        let stdin = child.stdin.take();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let writer = SessionWriter(Arc::new(Mutex::new(stdin)));
        let child = Arc::new(Mutex::new(child));

        if let Some(err) = stderr {
            std::thread::spawn(move || {
                let buf = BufReader::new(err);
                for line in buf.lines().map_while(Result::ok) {
                    tracing::debug!(target: "thegn::plugin", "stderr: {line}");
                }
            });
        }

        let reap = Arc::clone(&child);
        let reader_writer = writer.clone();
        std::thread::spawn(move || {
            if let Some(out) = stdout {
                let mut buf = BufReader::new(out);
                let mut line = Vec::new();
                loop {
                    line.clear();
                    let n = match buf
                        .by_ref()
                        .take(MAX_LINE_BYTES as u64)
                        .read_until(b'\n', &mut line)
                    {
                        Ok(0) | Err(_) => break,
                        Ok(n) => n,
                    };
                    let text = String::from_utf8_lossy(&line[..n]).trim().to_string();
                    if text.is_empty() {
                        continue;
                    }
                    on_event(parse_line(&text));
                }
            }
            // EOF: close our stdin handle (unblocks a child stuck on read),
            // reap, and deliver the exit exactly once.
            reader_writer.close();
            let code = {
                let mut guard = reap.lock().unwrap_or_else(|e| e.into_inner());
                guard.wait().ok().and_then(|s| s.code())
            };
            on_event(SessionEvent::Exit { code });
        });

        Ok(Self { child, writer })
    }

    /// The cloneable write half.
    pub fn writer(&self) -> SessionWriter {
        self.writer.clone()
    }

    /// Hard-stop the process (the reader thread then reaps it and delivers
    /// `Exit`). Used on shutdown after a best-effort `deactivate`.
    pub fn kill(&self) {
        self.writer.close();
        let mut guard = self.child.lock().unwrap_or_else(|e| e.into_inner());
        // best-effort: the process may already have exited.
        let _ = guard.kill(); // best-effort: the process may already have exited (see above)
    }
}

/// Classify one NDJSON line.
fn parse_line(text: &str) -> SessionEvent {
    // A response has an `id` and result/error but no `method`; try the
    // message shape first because it is the common case.
    match serde_json::from_str::<serde_json::Value>(text) {
        Ok(v) if v.get("method").is_some() => match serde_json::from_value::<RpcMessage>(v) {
            Ok(m) => SessionEvent::Message(m),
            Err(_) => SessionEvent::Junk(text.to_string()),
        },
        Ok(v) if v.get("id").is_some() => match serde_json::from_value::<RpcResponse>(v) {
            Ok(r) => SessionEvent::Response(r),
            Err(_) => SessionEvent::Junk(text.to_string()),
        },
        _ => SessionEvent::Junk(text.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    fn sh(script: &str) -> Vec<String> {
        vec!["sh".into(), "-c".into(), script.into()]
    }

    fn collect(rx: &mpsc::Receiver<SessionEvent>) -> Vec<SessionEvent> {
        let mut out = Vec::new();
        while let Ok(ev) = rx.recv_timeout(Duration::from_secs(10)) {
            let done = matches!(ev, SessionEvent::Exit { .. });
            out.push(ev);
            if done {
                break;
            }
        }
        out
    }

    #[test]
    fn round_trips_a_callback_and_classifies_lines() {
        let (tx, rx) = mpsc::channel();
        // The child echoes an update verb for every line it reads, plus one
        // junk line, then exits when stdin closes.
        let session = ResidentSession::spawn(
            &sh(r#"echo not-json; while read -r _; do echo '{"method":"update","params":{"surface":"s"}}'; break; done"#),
            &BTreeMap::new(),
            None,
            move |ev| {
                let _ = tx.send(ev); // best-effort: test receiver may be gone
            },
        )
        .unwrap();
        session
            .writer()
            .notify(PluginCallback::Render, serde_json::json!({}))
            .unwrap();
        session.writer().close();
        let events = collect(&rx);
        assert!(
            matches!(events.first(), Some(SessionEvent::Junk(j)) if j == "not-json"),
            "{events:?}"
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, SessionEvent::Message(m) if m.method.as_str() == "update")),
            "{events:?}"
        );
        assert!(
            matches!(events.last(), Some(SessionEvent::Exit { code: Some(0) })),
            "{events:?}"
        );
    }

    #[test]
    fn responses_are_classified_and_kill_delivers_exit() {
        let (tx, rx) = mpsc::channel();
        let session = ResidentSession::spawn(
            &sh(r#"echo '{"id":7,"result":{"ok":true}}'; sleep 30"#),
            &BTreeMap::new(),
            None,
            move |ev| {
                let _ = tx.send(ev); // best-effort: test receiver may be gone
            },
        )
        .unwrap();
        let first = rx.recv_timeout(Duration::from_secs(10)).unwrap();
        assert!(
            matches!(&first, SessionEvent::Response(r) if r.id == 7),
            "{first:?}"
        );
        session.kill();
        let events = collect(&rx);
        assert!(
            matches!(events.last(), Some(SessionEvent::Exit { .. })),
            "{events:?}"
        );
    }

    #[test]
    fn writes_to_a_dead_session_error() {
        let (tx, rx) = mpsc::channel();
        let session = ResidentSession::spawn(&sh("exit 3"), &BTreeMap::new(), None, move |ev| {
            let _ = tx.send(ev); // best-effort: test receiver may be gone (dead-session case)
        })
        .unwrap();
        let events = collect(&rx);
        assert!(
            matches!(events.last(), Some(SessionEvent::Exit { code: Some(3) })),
            "{events:?}"
        );
        // The reader closed the writer on EOF; a late notify errors cleanly.
        assert!(
            session
                .writer()
                .notify(PluginCallback::Render, serde_json::json!({}))
                .is_err()
        );
    }
}
