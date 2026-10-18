//! Sole process/pipe owner; cancellation never travels through the data queue.
use super::admission::Shared;
use super::{
    CLOSE_BUDGET, CloseReason, MAX_STDERR_LINE_BYTES, MAX_STDERR_TAIL_BYTES,
    STDERR_LOG_LINES_PER_WINDOW, SessionEvent, SessionOutcome, TreeGuarantee, junk_text,
};
use crate::plugin::platform::{Input, Output, Process};
use crate::plugin::proc::{FrameReject, MAX_LINE_BYTES};
use futures_util::FutureExt;
use std::collections::VecDeque;
use std::io;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Mutex};
use tokio::time::{Duration, Instant};

const STDERR_LOG_WINDOW: Duration = Duration::from_secs(1);
const STDERR_TRUNCATION_MARKER: &str = "...[truncated]";

struct StderrCapture {
    partial: Vec<u8>,
    truncating: bool,
    tail: VecDeque<u8>,
    bytes: u64,
    lines: u64,
    dropped_lines: u64,
    truncated_lines: u64,
    window_started: Instant,
    logged_in_window: u64,
}
impl StderrCapture {
    fn new() -> Self {
        Self {
            partial: Vec::with_capacity(MAX_STDERR_LINE_BYTES),
            truncating: false,
            tail: VecDeque::with_capacity(MAX_STDERR_TAIL_BYTES),
            bytes: 0,
            lines: 0,
            dropped_lines: 0,
            truncated_lines: 0,
            window_started: Instant::now(),
            logged_in_window: 0,
        }
    }

    fn feed(&mut self, bytes: &[u8]) {
        self.bytes = self.bytes.saturating_add(bytes.len() as u64);
        if bytes.len() >= MAX_STDERR_TAIL_BYTES {
            self.tail.clear();
            self.tail
                .extend(&bytes[bytes.len() - MAX_STDERR_TAIL_BYTES..]);
        } else {
            let excess = self
                .tail
                .len()
                .saturating_add(bytes.len())
                .saturating_sub(MAX_STDERR_TAIL_BYTES);
            self.tail.drain(..excess);
            self.tail.extend(bytes);
        }
        for &byte in bytes {
            if byte == b'\n' {
                self.finish_line();
            } else if !self.truncating && self.partial.len() < MAX_STDERR_LINE_BYTES {
                self.partial.push(byte);
            } else if !self.truncating {
                self.truncating = true;
            }
        }
    }

    fn finish_line(&mut self) {
        self.lines = self.lines.saturating_add(1);
        if self.truncating {
            self.truncated_lines = self.truncated_lines.saturating_add(1);
        }
        let now = Instant::now();
        if now.duration_since(self.window_started) >= STDERR_LOG_WINDOW {
            self.window_started = now;
            self.logged_in_window = 0;
        }
        if self.logged_in_window < STDERR_LOG_LINES_PER_WINDOW {
            self.logged_in_window += 1;
            let line = framed_stderr_line(&self.partial, self.truncating);
            tracing::debug!(target: "thegn::plugin", "stderr: {line}");
        } else {
            self.dropped_lines = self.dropped_lines.saturating_add(1);
        }
        self.partial.clear();
        self.truncating = false;
    }

    fn finish_partial(&mut self) {
        if !self.partial.is_empty() || self.truncating {
            self.finish_line();
        }
    }

    fn outcome_fields(&self) -> (Vec<u8>, u64, u64, u64, u64) {
        (
            self.tail.iter().copied().collect(),
            self.bytes,
            self.lines,
            self.dropped_lines,
            self.truncated_lines,
        )
    }
}

fn framed_stderr_line(bytes: &[u8], truncated: bool) -> String {
    let mut line = String::from_utf8_lossy(bytes).into_owned();
    if truncated {
        let content_limit = MAX_STDERR_LINE_BYTES - STDERR_TRUNCATION_MARKER.len();
        while !line.is_char_boundary(content_limit.min(line.len())) {
            line.pop();
        }
        line.truncate(content_limit.min(line.len()));
        line.push_str(STDERR_TRUNCATION_MARKER);
    }
    line
}

pub(super) struct HeldProcess {
    process: Process,
    stderr: StderrCapture,
}

struct Lines {
    partial: Vec<u8>,
    discarding: bool,
}
#[derive(Debug, Clone, Copy)]
enum LineReject {
    InvalidUtf8,
    Oversized,
}
impl Lines {
    fn new() -> Self {
        Self {
            partial: Vec::new(),
            discarding: false,
        }
    }
    fn feed(&mut self, bytes: &[u8], callback: &impl Fn(SessionEvent)) -> Result<(), CloseReason> {
        for &byte in bytes {
            if self.discarding {
                if byte == b'\n' {
                    self.discarding = false;
                }
                continue;
            }
            if byte == b'\n' {
                self.finish(callback)?;
                continue;
            }
            if self.partial.len() >= MAX_LINE_BYTES - 1 {
                self.emit_junk(LineReject::Oversized, callback)?;
                self.discarding = true;
                continue;
            }
            if self.partial.len() == self.partial.capacity() {
                let capacity = self
                    .partial
                    .capacity()
                    .saturating_mul(2)
                    .clamp(128, MAX_LINE_BYTES);
                self.partial.reserve_exact(capacity - self.partial.len());
            }
            self.partial.push(byte);
        }
        Ok(())
    }
    fn finish(&mut self, callback: &impl Fn(SessionEvent)) -> Result<(), CloseReason> {
        match std::str::from_utf8(&self.partial) {
            Ok(text) => {
                let text = text.trim();
                if !text.is_empty() {
                    catch_unwind(AssertUnwindSafe(|| callback(super::parse_line(text))))
                        .map_err(|_| CloseReason::CallbackPanic)?;
                }
                self.partial.clear();
                Ok(())
            }
            Err(_) => self.emit_junk(LineReject::InvalidUtf8, callback),
        }
    }
    fn emit_junk(
        &mut self,
        reason: LineReject,
        callback: &impl Fn(SessionEvent),
    ) -> Result<(), CloseReason> {
        let text = junk_text(&self.partial);
        self.partial = Vec::with_capacity(128);
        match reason {
            LineReject::InvalidUtf8 => tracing::debug!(
                target: "thegn::plugin",
                reason = ?FrameReject::InvalidUtf8,
                "discarding rejected resident plugin frame"
            ),
            LineReject::Oversized => tracing::debug!(
                target: "thegn::plugin",
                reason = ?reason,
                "discarding oversized resident plugin frame"
            ),
        }
        if !text.is_empty() || matches!(reason, LineReject::Oversized) {
            catch_unwind(AssertUnwindSafe(|| callback(SessionEvent::Junk(text))))
                .map_err(|_| CloseReason::CallbackPanic)?;
        }
        Ok(())
    }
}
async fn read(pipe: &mut Option<Output>, bytes: &mut [u8], live: bool) -> io::Result<usize> {
    if live && let Some(pipe) = pipe {
        return pipe.read(bytes).await;
    }
    std::future::pending().await
}
async fn write(
    pipe: &mut Option<Input>,
    active: &Option<(Vec<u8>, usize, Instant)>,
) -> io::Result<usize> {
    if let (Some(pipe), Some((bytes, offset, _))) = (pipe, active) {
        return pipe.write(&bytes[*offset..]).await;
    }
    std::future::pending().await
}

async fn running(
    process: &mut Process,
    shared: &Shared,
    lines: &mut Lines,
    stderr: &mut StderrCapture,
    callback: &impl Fn(SessionEvent),
) -> CloseReason {
    if !process.errors.is_empty() {
        return CloseReason::Protocol;
    }
    let mut output = [0; 8192];
    let mut error = [0; 8192];
    let mut stderr_live = true;
    let mut active: Option<(Vec<u8>, usize, Instant)> = None;
    let mut stdin_closed = false;
    loop {
        // Register before inspecting the latch/queue to avoid losing a close or
        // first frame between inspecting state and going to sleep.
        let wake = shared.wake.notified();
        tokio::pin!(wake);
        wake.as_mut().enable();
        let closing = shared.closing();
        if let Some((reason, _)) = closing
            && reason != CloseReason::WriterClosed
        {
            return reason;
        }
        if active.is_none() && !stdin_closed {
            active = shared
                .pop()
                .map(|bytes| (bytes, 0, Instant::now() + CLOSE_BUDGET));
        }
        if active.is_none()
            && let Some((_, deadline)) = closing
            && !stdin_closed
        {
            if !process.close_stdin(deadline).await {
                return CloseReason::WriteTimeout;
            }
            stdin_closed = true;
        }
        let deadline = active
            .as_ref()
            .map(|(_, _, deadline)| *deadline)
            .into_iter()
            .chain(closing.map(|(_, deadline)| deadline))
            .min();
        let expired = async {
            if let Some(deadline) = deadline {
                tokio::time::sleep_until(deadline).await;
            } else {
                std::future::pending::<()>().await;
            }
        };
        tokio::select! {
            biased;
            _ = &mut wake => {}
            _ = expired => return CloseReason::WriteTimeout,
            ready = process.leader.wait_ready() => {
                if let Err(error) = ready { process.errors.push(format!("leader observation: {error}")); }
                return CloseReason::ProcessExit;
            }
            result = read(&mut process.stdout, &mut output, true) => match result {
                Ok(0) => return CloseReason::StdoutClosed,
                Ok(len) => if let Err(reason) = lines.feed(&output[..len], callback) { return reason; },
                Err(error) => { process.errors.push(format!("stdout read: {error}")); return CloseReason::Protocol; }
            },
            result = read(&mut process.stderr, &mut error, stderr_live) => match result {
                Ok(0) => stderr_live = false,
                Ok(len) => stderr.feed(&error[..len]),
                Err(error) => { process.errors.push(format!("stderr read: {error}")); return CloseReason::Protocol; }
            },
            result = write(&mut process.stdin, &active) => match result {
                Ok(0) => return CloseReason::WriteFailed,
                Ok(len) => if let Some((bytes, offset, _)) = &mut active {
                    *offset += len;
                    if *offset == bytes.len() { active = None; }
                },
                Err(error) => { process.errors.push(format!("stdin write: {error}")); return CloseReason::WriteFailed; }
            },
        }
        // Sustained output must not monopolize an executor worker. This is
        // useful-work yielding; resident idle has no timer.
        tokio::task::yield_now().await;
    }
}

async fn cleanup(
    process: &mut Process,
    shared: &Shared,
    lines: &mut Lines,
    stderr: &mut StderrCapture,
    callback: &impl Fn(SessionEvent),
    reason: CloseReason,
) -> SessionOutcome {
    shared.close(reason, Instant::now() + CLOSE_BUDGET);
    shared.discard();
    let deadline = shared.closing().expect("closing latch").1;
    // EOF frequently precedes the kernel's exit notification by a few cycles.
    // A short closing-only grace preserves the child's actual status; it cannot
    // turn an inherited-pipe hang into an unbounded wait.
    if reason == CloseReason::StdoutClosed {
        drop(
            tokio::time::timeout_at(
                (Instant::now() + Duration::from_millis(20)).min(deadline),
                process.leader.wait_ready(),
            )
            .await,
        );
    }
    let already_reaped = process.leader.reaped_status();
    let termination_requested = if already_reaped.is_some() {
        true
    } else {
        match process.leader.terminate() {
            Ok(()) => true,
            Err(error) => {
                process.errors.push(format!("termination: {error}"));
                false
            }
        }
    };
    // Drain already written output after leader exit (without losing the final
    // response), but bound inherited-pipe drainage independently of reaping.
    let drain_until = (Instant::now() + Duration::from_millis(100)).min(deadline);
    let mut output = [0; 8192];
    let mut error = [0; 8192];
    let mut stdout_live = process.stdout.is_some();
    let mut stderr_live = process.stderr.is_some();
    let mut callbacks_live =
        reason != CloseReason::CallbackPanic && reason != CloseReason::OwnerPanic;
    while stdout_live || stderr_live {
        tokio::select! {
            biased;
            _ = tokio::time::sleep_until(drain_until) => break,
            result = read(&mut process.stdout, &mut output, stdout_live) => match result {
                Ok(0) => { stdout_live = false; if callbacks_live && lines.finish(callback).is_err() { callbacks_live = false; } }
                Ok(len) => if callbacks_live && lines.feed(&output[..len], callback).is_err() { callbacks_live = false; },
                Err(_) => stdout_live = false,
            },
            result = read(&mut process.stderr, &mut error, stderr_live) => match result {
                Ok(0) | Err(_) => stderr_live = false,
                Ok(len) => stderr.feed(&error[..len]),
            },
        }
        tokio::task::yield_now().await;
    }
    stderr.finish_partial();
    shared.finish_close();
    // Pipe cancellation and leader observation share the SAME deadline. Start
    // cancellation before waiting so inherited Windows pipes cannot consume
    // the entire deadline. Requests never count as confirmed completion.
    let pipes_settled = process.settle_pipes(deadline).await;
    if !pipes_settled {
        process
            .errors
            .push("pipe settlement incomplete at deadline".into());
    }
    let mut code = already_reaped.and_then(|status| status.code());
    let mut leader_reaped = already_reaped.is_some();
    if !leader_reaped && termination_requested {
        match tokio::time::timeout_at(deadline, process.leader.wait_ready()).await {
            Ok(Ok(())) => match process.leader.reap() {
                Ok(status) => {
                    code = status.code();
                    leader_reaped = true;
                }
                Err(error) => process.errors.push(format!("leader reap: {error}")),
            },
            Ok(Err(error)) => process.errors.push(format!("leader wait: {error}")),
            Err(_) => process.errors.push("leader wait deadline elapsed".into()),
        }
    }
    if callbacks_live
        && catch_unwind(AssertUnwindSafe(|| callback(SessionEvent::Exit { code }))).is_err()
    {
        process.errors.push("terminal callback panicked".into());
    }
    let (stderr_tail, stderr_bytes, stderr_lines, stderr_dropped_lines, stderr_truncated_lines) =
        stderr.outcome_fields();
    SessionOutcome {
        reason,
        leader_reaped,
        code,
        pipes_settled,
        tree: TreeGuarantee::Unproven,
        termination_requested,
        errors: std::mem::take(&mut process.errors),
        stderr_tail,
        stderr_bytes,
        stderr_lines,
        stderr_dropped_lines,
        stderr_truncated_lines,
    }
}

/// Constructed before handing the future to Tokio. Even an abort before first
/// poll transfers the still-owned process to the registry's held slot.
pub(super) struct Custody {
    process: Option<Process>,
    shared: Arc<Shared>,
    held: Arc<Mutex<Option<HeldProcess>>>,
    stderr: StderrCapture,
}
impl Custody {
    pub fn new(
        process: Process,
        shared: Arc<Shared>,
        held: Arc<Mutex<Option<HeldProcess>>>,
    ) -> Self {
        Self {
            process: Some(process),
            shared,
            held,
            stderr: StderrCapture::new(),
        }
    }
    pub(super) fn from_held(
        held_process: HeldProcess,
        shared: Arc<Shared>,
        held: Arc<Mutex<Option<HeldProcess>>>,
    ) -> Self {
        Self {
            process: Some(held_process.process),
            shared,
            held,
            stderr: held_process.stderr,
        }
    }
    fn finish(&mut self, outcome: SessionOutcome) {
        let process = self.process.take();
        if !outcome.settled() {
            if let Some(process) = process {
                *self.held.lock().unwrap_or_else(|e| e.into_inner()) = Some(HeldProcess {
                    process,
                    stderr: std::mem::replace(&mut self.stderr, StderrCapture::new()),
                });
            }
        }
        self.shared.outcome.send_replace(Some(outcome));
    }
}
impl Drop for Custody {
    fn drop(&mut self) {
        if let Some(process) = self.process.take() {
            let status = process.leader.reaped_status();
            let outcome = SessionOutcome {
                reason: CloseReason::OwnerPanic,
                leader_reaped: status.is_some(),
                code: status.and_then(|status| status.code()),
                pipes_settled: false,
                tree: TreeGuarantee::Unproven,
                termination_requested: false,
                errors: vec!["lifecycle task cancelled or unwound with owned resources".into()],
                stderr_tail: self.stderr.outcome_fields().0,
                stderr_bytes: self.stderr.bytes,
                stderr_lines: self.stderr.lines,
                stderr_dropped_lines: self.stderr.dropped_lines,
                stderr_truncated_lines: self.stderr.truncated_lines,
            };
            *self.held.lock().unwrap_or_else(|e| e.into_inner()) = Some(HeldProcess {
                process,
                stderr: std::mem::replace(&mut self.stderr, StderrCapture::new()),
            });
            self.shared
                .close(CloseReason::OwnerPanic, Instant::now() + CLOSE_BUDGET);
            self.shared.finish_close();
            self.shared.outcome.send_replace(Some(outcome));
        }
    }
}

pub(super) async fn run(mut custody: Custody, callback: impl Fn(SessionEvent) + Send + Sync) {
    let shared = custody.shared.clone();
    let routed = |event| {
        if let SessionEvent::Response(response) = &event
            && shared.route_response(response)
        {
            return;
        }
        callback(event);
    };
    let mut lines = Lines::new();
    let process = custody.process.as_mut().expect("owned process");
    let reason = AssertUnwindSafe(running(
        process,
        &shared,
        &mut lines,
        &mut custody.stderr,
        &routed,
    ))
    .catch_unwind()
    .await
    .unwrap_or(CloseReason::OwnerPanic);
    let outcome = cleanup(
        process,
        &shared,
        &mut lines,
        &mut custody.stderr,
        &routed,
        reason,
    )
    .await;
    custody.finish(outcome);
}

/// Final cleanup retry uses only the objects retained by the original owner.
/// A reaped Unix leader is never signaled again; no PID-number fallback exists.
pub(super) async fn retry(mut custody: Custody, deadline: Instant) {
    let shared = custody.shared.clone();
    // A prior per-session deadline may already have elapsed. This is a fresh,
    // explicit application shutdown attempt against its single deadline.
    shared
        .state
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .closing = Some((CloseReason::Shutdown, deadline));
    let outcome = cleanup(
        custody.process.as_mut().expect("held process"),
        &shared,
        &mut Lines::new(),
        &mut custody.stderr,
        &|_| {},
        CloseReason::Shutdown,
    )
    .await;
    custody.finish(outcome);
}

#[cfg(test)]
mod stderr_tests {
    use super::{StderrCapture, framed_stderr_line};
    use crate::plugin::session::{
        MAX_STDERR_LINE_BYTES, MAX_STDERR_TAIL_BYTES, STDERR_LOG_LINES_PER_WINDOW,
    };

    #[test]
    fn newline_free_flood_counts_every_byte_and_keeps_only_the_tail() {
        let mut capture = StderrCapture::new();
        let block = [b'x'; 8192];
        for _ in 0..8192 {
            capture.feed(&block);
        }
        capture.finish_partial();

        assert_eq!(capture.bytes, 64 * 1024 * 1024);
        assert_eq!(capture.tail.len(), MAX_STDERR_TAIL_BYTES);
        assert_eq!(capture.lines, 1);
        assert_eq!(capture.truncated_lines, 1);
    }

    #[test]
    fn fast_lines_obey_the_fixed_log_budget_and_count_drops() {
        let mut capture = StderrCapture::new();
        for _ in 0..100 {
            capture.feed(b"line\n");
        }

        assert_eq!(capture.lines, 100);
        assert_eq!(capture.dropped_lines, 100 - STDERR_LOG_LINES_PER_WINDOW);
    }

    #[test]
    fn oversized_line_has_a_bounded_truncation_marker() {
        let mut capture = StderrCapture::new();
        capture.feed(&vec![b'x'; MAX_STDERR_LINE_BYTES + 100]);
        let rendered = framed_stderr_line(&capture.partial, capture.truncating);
        assert!(capture.truncating);
        assert!(rendered.ends_with("...[truncated]"));
        assert!(rendered.len() <= MAX_STDERR_LINE_BYTES);
        capture.feed(b"\nnext\n");
        assert_eq!(capture.lines, 2);
        assert_eq!(capture.truncated_lines, 1);
        assert_eq!(capture.partial, b"");
    }
}
