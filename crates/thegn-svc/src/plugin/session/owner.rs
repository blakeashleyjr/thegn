//! Sole process/pipe owner; cancellation never travels through the data queue.
use super::admission::Shared;
use super::{CLOSE_BUDGET, CloseReason, SessionEvent, SessionOutcome, TreeGuarantee};
use crate::plugin::platform::{Input, Output, Process};
use crate::plugin::proc::MAX_LINE_BYTES;
use futures_util::FutureExt;
use std::io;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Mutex};
use tokio::time::{Duration, Instant};

struct Lines {
    partial: Vec<u8>,
}
impl Lines {
    fn new() -> Self {
        Self {
            partial: Vec::new(),
        }
    }
    fn feed(&mut self, bytes: &[u8], callback: &impl Fn(SessionEvent)) -> Result<(), CloseReason> {
        for &byte in bytes {
            if byte == b'\n' {
                self.finish(callback)?;
                continue;
            }
            if self.partial.len() >= MAX_LINE_BYTES - 1 {
                return Err(CloseReason::Protocol);
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
        let text = std::str::from_utf8(&self.partial)
            .map_err(|_| CloseReason::Protocol)?
            .trim();
        if !text.is_empty() {
            catch_unwind(AssertUnwindSafe(|| callback(super::parse_line(text))))
                .map_err(|_| CloseReason::CallbackPanic)?;
        }
        self.partial.clear();
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
                Ok(len) => tracing::debug!(target: "thegn::plugin", "stderr: {}", String::from_utf8_lossy(&error[..len])),
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
                Ok(len) => tracing::debug!(target: "thegn::plugin", "stderr: {}", String::from_utf8_lossy(&error[..len])),
            },
        }
        tokio::task::yield_now().await;
    }
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
    SessionOutcome {
        reason,
        leader_reaped,
        code,
        pipes_settled,
        tree: TreeGuarantee::Unproven,
        termination_requested,
        errors: std::mem::take(&mut process.errors),
    }
}

/// Constructed before handing the future to Tokio. Even an abort before first
/// poll transfers the still-owned process to the registry's held slot.
pub(super) struct Custody {
    process: Option<Process>,
    shared: Arc<Shared>,
    held: Arc<Mutex<Option<Process>>>,
}
impl Custody {
    pub fn new(process: Process, shared: Arc<Shared>, held: Arc<Mutex<Option<Process>>>) -> Self {
        Self {
            process: Some(process),
            shared,
            held,
        }
    }
    fn finish(&mut self, outcome: SessionOutcome) {
        let process = self.process.take();
        if !outcome.settled() {
            *self.held.lock().unwrap_or_else(|e| e.into_inner()) = process;
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
            };
            *self.held.lock().unwrap_or_else(|e| e.into_inner()) = Some(process);
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
    let reason = AssertUnwindSafe(running(process, &shared, &mut lines, &routed))
        .catch_unwind()
        .await
        .unwrap_or(CloseReason::OwnerPanic);
    let outcome = cleanup(process, &shared, &mut lines, &routed, reason).await;
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
        &|_| {},
        CloseReason::Shutdown,
    )
    .await;
    custody.finish(outcome);
}
