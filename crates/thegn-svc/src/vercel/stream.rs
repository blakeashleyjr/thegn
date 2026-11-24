//! Stream mode for a Vercel sandbox: a detached command, the NDJSON log
//! follow, and stdin over `POST …/stdin` (no resize, no raw TUI). The fallback
//! when `/interactive` is refused.
//!
//! stdout and stderr are merged into [`ExecFrame::Stdout`] in log order.
//! `Close` (or the caller dropping the control sender) kills the command with
//! SIGTERM, then SIGKILL after `KILL_GRACE`. The driver calls the control
//! plane after `open_stream` returns, so it needs an owned provider: the
//! receiver is `Arc<Self>`.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use thegn_core::vercel::admit::valid_sandbox_name;
use thegn_core::vercel::api::{self, LogLine};

use super::VercelProvider;
use super::client::{LogStream, WAIT_COMMAND_DEADLINE};
use super::pty::{CHANNEL_CAPACITY, send_chunked};
use crate::provider::{EXEC_EXIT_STATUS_UNKNOWN, ExecControl, ExecFrame, ExecSession, ExecSpec};

/// SIGTERM → SIGKILL grace after a close.
pub(super) const KILL_GRACE: Duration = Duration::from_secs(5);
const SIGTERM: i32 = 15;
const SIGKILL: i32 = 9;

/// The control-plane calls the stream driver makes for one running command.
/// A seam so the driver is tested against an in-memory fake.
pub(super) trait StreamOps: Send + Sync + 'static {
    /// Write stdin bytes (the client base64-encodes them on the wire).
    fn stdin(&self, data: Vec<u8>) -> impl Future<Output = Result<()>> + Send;
    fn kill(&self, signal: i32) -> impl Future<Output = Result<()>> + Send;
    /// The command's exit code once the log stream has ended (`None` =
    /// unknown).
    fn wait_exit(&self) -> impl Future<Output = Result<Option<i32>>> + Send;
}

/// The production [`StreamOps`]: the provider's client, bound to one session
/// and command.
struct ClientOps {
    provider: Arc<VercelProvider>,
    session: String,
    cmd: String,
}

impl StreamOps for ClientOps {
    async fn stdin(&self, data: Vec<u8>) -> Result<()> {
        self.provider
            .client
            .stdin(&self.session, &self.cmd, &data, false)
            .await
    }

    async fn kill(&self, signal: i32) -> Result<()> {
        self.provider
            .client
            .kill(&self.session, &self.cmd, signal)
            .await
    }

    async fn wait_exit(&self) -> Result<Option<i32>> {
        // The log stream ended without a `Command` line carrying the exit:
        // ask the control plane, under the client's hard overall deadline.
        self.provider
            .client
            .wait_command(&self.session, &self.cmd, WAIT_COMMAND_DEADLINE)
            .await
    }
}

impl VercelProvider {
    /// Open a stream-mode session: run `spec.argv` detached and follow its
    /// logs. Resumes a stopped sandbox. `spec.tty`, `cols` and `rows` are
    /// ignored (there is no PTY). `spec.env` passes the provider allowlist.
    pub async fn open_stream(self: Arc<Self>, name: &str, spec: &ExecSpec) -> Result<ExecSession> {
        anyhow::ensure!(valid_sandbox_name(name), "vercel: invalid sandbox name");
        let body = self.command_body(&spec.argv, spec.cwd.as_deref(), &spec.env, false)?;
        let body = api::with_attach_stdin(body);
        let mut session = self.interactive_session(name).await?;
        let cmd = match self.api().run_detached(&session, &body).await {
            Ok(cmd) => cmd,
            Err(error) if Self::is_session_stopped(&error) => {
                self.forget_session(name);
                session = self.interactive_session(name).await?;
                self.api().run_detached(&session, &body).await?
            }
            Err(error) => return Err(error),
        };
        let logs = match self.api().logs(&session, &cmd).await {
            Ok(logs) => logs,
            Err(e) => {
                // Nothing will ever read this command: don't leave it running.
                if let Err(kill) = self.client.kill(&session, &cmd, SIGKILL).await {
                    tracing::debug!(target: "thegn::provider", error = %kill, "vercel orphan kill failed");
                }
                return Err(e);
            }
        };
        let ops = ClientOps {
            provider: Arc::clone(&self),
            session: session.clone(),
            cmd,
        };
        Ok(spawn_stream(ops, logs, session))
    }
}

/// Spawn [`drive_stream`] and hand back the channel handle.
pub(super) fn spawn_stream<O: StreamOps>(
    ops: O,
    logs: LogStream,
    session_id: String,
) -> ExecSession {
    let (frames_tx, frames_rx) = tokio::sync::mpsc::channel::<ExecFrame>(CHANNEL_CAPACITY);
    let (control_tx, control_rx) = tokio::sync::mpsc::channel::<ExecControl>(256);
    let session_rx = tokio::sync::watch::channel(Some(session_id)).1;
    tokio::spawn(drive_stream(ops, logs, frames_tx, control_rx));
    ExecSession {
        frames: frames_rx,
        control: control_tx,
        session_id: session_rx,
    }
}

/// What one log line means for the session.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum LogStep {
    Output(Vec<u8>),
    Exit(i32),
    /// The log stream reported an error: the stream is lost.
    Lost,
    Ignore,
}

/// Classify one log line: stdout/stderr merge into output; a command line
/// with an exit code ends the session.
pub(super) fn log_step(line: LogLine) -> LogStep {
    match line {
        LogLine::Stdout(bytes) | LogLine::Stderr(bytes) => LogStep::Output(bytes),
        LogLine::Command {
            exit_code: Some(code),
            ..
        } => LogStep::Exit(code),
        LogLine::Command {
            exit_code: None, ..
        } => LogStep::Ignore,
        LogLine::Error { .. } => LogStep::Lost,
    }
}

async fn send_exit(frames_tx: &tokio::sync::mpsc::Sender<ExecFrame>, code: i32) {
    if frames_tx.send(ExecFrame::Exit(code)).await.is_err() {
        tracing::debug!(target: "thegn::provider", "vercel stream exit had no consumer");
    }
}

/// Pump one detached command: log lines → frames, stdin → `POST …/stdin`,
/// resize ignored, close → SIGTERM then SIGKILL. A log stream that ends
/// cleanly asks [`StreamOps::wait_exit`] for the code; an errored log stream
/// closes the frames without an exit (the bridge decides).
pub(super) async fn drive_stream<O: StreamOps>(
    ops: O,
    mut logs: LogStream,
    frames_tx: tokio::sync::mpsc::Sender<ExecFrame>,
    mut control_rx: tokio::sync::mpsc::Receiver<ExecControl>,
) {
    loop {
        tokio::select! {
            line = logs.receiver_mut().recv() => match line.map(log_step) {
                Some(LogStep::Output(bytes)) => {
                    if !send_chunked(&frames_tx, bytes).await {
                        // The consumer is gone: stop the command.
                        terminate(&ops, logs.receiver_mut(), &frames_tx).await;
                        return;
                    }
                }
                Some(LogStep::Exit(code)) => {
                    send_exit(&frames_tx, code).await;
                    return;
                }
                Some(LogStep::Lost) => return,
                Some(LogStep::Ignore) => {}
                None => {
                    let code = match ops.wait_exit().await {
                        Ok(Some(code)) => code,
                        Ok(None) => EXEC_EXIT_STATUS_UNKNOWN,
                        Err(e) => {
                            tracing::debug!(target: "thegn::provider", error = %e, "vercel stream exit wait failed");
                            EXEC_EXIT_STATUS_UNKNOWN
                        }
                    };
                    send_exit(&frames_tx, code).await;
                    return;
                }
            },
            ctrl = control_rx.recv() => match ctrl {
                Some(ExecControl::Stdin(bytes)) => {
                    if let Err(e) = ops.stdin(bytes).await {
                        tracing::debug!(target: "thegn::provider", error = %e, "vercel stream stdin failed");
                    }
                }
                Some(ExecControl::Resize { .. }) => {}
                Some(ExecControl::Close) | None => {
                    terminate(&ops, logs.receiver_mut(), &frames_tx).await;
                    return;
                }
            },
        }
    }
}

/// SIGTERM, wait up to `KILL_GRACE` for the command to end (forwarding its
/// last output and exit), then SIGKILL.
async fn terminate<O: StreamOps>(
    ops: &O,
    logs: &mut tokio::sync::mpsc::Receiver<LogLine>,
    frames_tx: &tokio::sync::mpsc::Sender<ExecFrame>,
) {
    if let Err(e) = ops.kill(SIGTERM).await {
        tracing::debug!(target: "thegn::provider", error = %e, "vercel stream SIGTERM failed");
    }
    let ended = tokio::time::timeout(KILL_GRACE, async {
        while let Some(line) = logs.recv().await {
            match log_step(line) {
                LogStep::Output(bytes) => {
                    // Best effort: the consumer may already be gone.
                    send_chunked(frames_tx, bytes).await;
                }
                LogStep::Exit(code) => {
                    send_exit(frames_tx, code).await;
                    return;
                }
                LogStep::Lost | LogStep::Ignore => {}
            }
        }
    })
    .await;
    if ended.is_err()
        && let Err(e) = ops.kill(SIGKILL).await
    {
        tracing::debug!(target: "thegn::provider", error = %e, "vercel stream SIGKILL failed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Call {
        Stdin(Vec<u8>),
        Kill(i32),
        Wait,
    }

    #[derive(Clone, Default)]
    struct Fake {
        calls: Arc<Mutex<Vec<Call>>>,
        exit: Option<i32>,
    }

    impl Fake {
        fn calls(&self) -> Vec<Call> {
            self.calls.lock().expect("calls").clone()
        }
        fn push(&self, call: Call) {
            self.calls.lock().expect("calls").push(call);
        }
    }

    impl StreamOps for Fake {
        async fn stdin(&self, data: Vec<u8>) -> Result<()> {
            self.push(Call::Stdin(data));
            Ok(())
        }
        async fn kill(&self, signal: i32) -> Result<()> {
            self.push(Call::Kill(signal));
            Ok(())
        }
        async fn wait_exit(&self) -> Result<Option<i32>> {
            self.push(Call::Wait);
            Ok(self.exit)
        }
    }

    async fn drain(sess: &mut ExecSession) -> (Vec<u8>, Option<i32>) {
        let mut out = Vec::new();
        while let Some(frame) = sess.frames.recv().await {
            match frame {
                ExecFrame::Stdout(b) => out.extend(b),
                ExecFrame::Exit(code) => return (out, Some(code)),
            }
        }
        (out, None)
    }

    #[tokio::test]
    async fn logs_merge_stdout_and_stderr_in_order_then_wait_for_the_exit() {
        let fake = Fake {
            exit: Some(3),
            ..Fake::default()
        };
        let (log_tx, log_rx) = tokio::sync::mpsc::channel(8);
        let mut sess = spawn_stream(
            fake.clone(),
            LogStream::from_receiver(log_rx),
            "sess_9".into(),
        );
        assert_eq!(sess.session_id.borrow().as_deref(), Some("sess_9"));
        for line in [
            LogLine::Stdout(b"a".to_vec()),
            LogLine::Stderr(b"b".to_vec()),
            LogLine::Command {
                id: "cmd_1".into(),
                exit_code: None,
            },
            LogLine::Stdout(b"c".to_vec()),
        ] {
            log_tx.send(line).await.expect("log");
        }
        drop(log_tx);
        assert_eq!(drain(&mut sess).await, (b"abc".to_vec(), Some(3)));
        assert_eq!(fake.calls(), vec![Call::Wait]);
    }

    #[tokio::test]
    async fn a_command_line_with_an_exit_code_ends_the_session() {
        let fake = Fake::default();
        let (log_tx, log_rx) = tokio::sync::mpsc::channel(8);
        let mut sess = spawn_stream(fake.clone(), LogStream::from_receiver(log_rx), "s".into());
        log_tx
            .send(LogLine::Command {
                id: "cmd_1".into(),
                exit_code: Some(0),
            })
            .await
            .expect("log");
        assert_eq!(drain(&mut sess).await, (Vec::new(), Some(0)));
        assert!(fake.calls().is_empty(), "no wait poll needed");
    }

    #[tokio::test]
    async fn unknown_exit_after_a_clean_log_end_is_the_sentinel() {
        let fake = Fake::default();
        let (log_tx, log_rx) = tokio::sync::mpsc::channel(8);
        let mut sess = spawn_stream(fake, LogStream::from_receiver(log_rx), "s".into());
        drop(log_tx);
        assert_eq!(
            drain(&mut sess).await,
            (Vec::new(), Some(EXEC_EXIT_STATUS_UNKNOWN))
        );
    }

    #[tokio::test]
    async fn a_log_error_closes_the_frames_without_an_exit() {
        let fake = Fake::default();
        let (log_tx, log_rx) = tokio::sync::mpsc::channel(8);
        let mut sess = spawn_stream(fake, LogStream::from_receiver(log_rx), "s".into());
        log_tx
            .send(LogLine::Error {
                code: "x".into(),
                message: "y".into(),
            })
            .await
            .expect("log");
        assert_eq!(drain(&mut sess).await, (Vec::new(), None));
    }

    #[tokio::test]
    async fn stdin_is_forwarded_and_resize_is_ignored() {
        let fake = Fake {
            exit: Some(0),
            ..Fake::default()
        };
        let (log_tx, log_rx) = tokio::sync::mpsc::channel(8);
        let mut sess = spawn_stream(fake.clone(), LogStream::from_receiver(log_rx), "s".into());
        sess.control
            .send(ExecControl::Stdin(b"hi\n".to_vec()))
            .await
            .expect("stdin");
        sess.control
            .send(ExecControl::Resize { cols: 1, rows: 1 })
            .await
            .expect("resize");
        sess.control
            .send(ExecControl::Stdin(b"\x00\xff".to_vec()))
            .await
            .expect("stdin");
        // Let the driver handle the controls before the logs end.
        for _ in 0..10_000 {
            if fake.calls().len() >= 2 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(fake.calls().len(), 2, "both stdin writes reached the fake");
        drop(log_tx);
        assert_eq!(drain(&mut sess).await.1, Some(0));
        assert_eq!(
            fake.calls(),
            vec![
                Call::Stdin(b"hi\n".to_vec()),
                Call::Stdin(b"\x00\xff".to_vec()),
                Call::Wait
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn close_sends_sigterm_then_sigkill_after_the_grace() {
        let fake = Fake::default();
        let (log_tx, log_rx) = tokio::sync::mpsc::channel(8);
        let mut sess = spawn_stream(fake.clone(), LogStream::from_receiver(log_rx), "s".into());
        sess.control.send(ExecControl::Close).await.expect("close");
        // The command never ends: the paused clock auto-advances past the grace.
        assert_eq!(drain(&mut sess).await, (Vec::new(), None));
        assert_eq!(fake.calls(), vec![Call::Kill(15), Call::Kill(9)]);
        drop(log_tx);
    }

    #[tokio::test(start_paused = true)]
    async fn close_skips_sigkill_when_the_command_exits_in_the_grace() {
        let fake = Fake::default();
        let (log_tx, log_rx) = tokio::sync::mpsc::channel(8);
        let mut sess = spawn_stream(fake.clone(), LogStream::from_receiver(log_rx), "s".into());
        sess.control.send(ExecControl::Close).await.expect("close");
        for _ in 0..10_000 {
            if fake.calls() == vec![Call::Kill(15)] {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(
            fake.calls(),
            vec![Call::Kill(15)],
            "SIGTERM before the logs"
        );
        log_tx
            .send(LogLine::Stdout(b"bye".to_vec()))
            .await
            .expect("log");
        log_tx
            .send(LogLine::Command {
                id: "c".into(),
                exit_code: Some(143),
            })
            .await
            .expect("log");
        assert_eq!(drain(&mut sess).await, (b"bye".to_vec(), Some(143)));
        assert_eq!(fake.calls(), vec![Call::Kill(15)]);
    }

    #[tokio::test(start_paused = true)]
    async fn dropping_the_control_sender_also_terminates() {
        let fake = Fake::default();
        let (log_tx, log_rx) = tokio::sync::mpsc::channel(8);
        let sess = spawn_stream(fake.clone(), LogStream::from_receiver(log_rx), "s".into());
        let ExecSession {
            mut frames,
            control,
            ..
        } = sess;
        drop(control);
        while frames.recv().await.is_some() {}
        assert_eq!(fake.calls(), vec![Call::Kill(15), Call::Kill(9)]);
        drop(log_tx);
    }

    #[test]
    fn log_step_merges_streams_and_reads_exits() {
        assert_eq!(
            log_step(LogLine::Stderr(b"e".to_vec())),
            LogStep::Output(b"e".to_vec())
        );
        assert_eq!(
            log_step(LogLine::Command {
                id: "c".into(),
                exit_code: Some(2)
            }),
            LogStep::Exit(2)
        );
        assert_eq!(
            log_step(LogLine::Error {
                code: "c".into(),
                message: "m".into()
            }),
            LogStep::Lost
        );
    }
}
