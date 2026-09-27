//! Bounded streaming output and owned cancellation for merge gate commands.
//!
//! The configured shell argv is supplied unchanged. Capture is bounded per
//! channel; process waits and pipe readers are kept off the UI loop by the
//! integrate worker. OS calls and filesystem operations themselves are not
//! preemptible.

use std::collections::VecDeque;
use std::io;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{
    Arc, OnceLock,
    atomic::{AtomicBool, AtomicUsize, Ordering},
    mpsc,
};
use std::time::{Duration, Instant};

const TAIL_BYTES: usize = 64 * 1024;
const READ_BYTES: usize = 8192;
const POLL: Duration = Duration::from_millis(8);
const TERM_GRACE: Duration = Duration::from_millis(250);
const REAP_GRACE: Duration = Duration::from_secs(2);
type ReapOwner = (Child, crate::platform::GroupHandle);
static REAPER_PENDING: AtomicUsize = AtomicUsize::new(0);
static REAPER: OnceLock<Result<mpsc::Sender<ReapOwner>, String>> = OnceLock::new();

#[derive(Debug)]
pub(super) enum CaptureResult {
    Completed { status: ExitStatus, log: String },
    Timeout { log: String },
    Infrastructure { reason: String, log: String },
    Poisoned { reason: String, log: String },
}

#[derive(Default)]
struct ByteTail {
    bytes: VecDeque<u8>,
    truncated: bool,
}

impl ByteTail {
    fn push(&mut self, bytes: &[u8]) {
        if bytes.len() >= TAIL_BYTES {
            self.truncated |= !self.bytes.is_empty() || bytes.len() > TAIL_BYTES;
            self.bytes.clear();
            self.bytes.extend(&bytes[bytes.len() - TAIL_BYTES..]);
            return;
        }
        let excess = self
            .bytes
            .len()
            .saturating_add(bytes.len())
            .saturating_sub(TAIL_BYTES);
        if excess != 0 {
            self.truncated = true;
            self.bytes.drain(..excess);
        }
        self.bytes.extend(bytes);
    }

    fn result(mut self) -> TailResult {
        TailResult {
            text: String::from_utf8_lossy(self.bytes.make_contiguous()).into_owned(),
            truncated: self.truncated,
        }
    }
}

#[derive(Default)]
struct TailResult {
    text: String,
    truncated: bool,
}

struct StreamResult {
    stdout: TailResult,
    stderr: TailResult,
    out_result: io::Result<()>,
    err_result: io::Result<()>,
}

struct Reader {
    receiver: mpsc::Receiver<io::Result<TailResult>>,
    join: std::thread::JoinHandle<()>,
}

fn spawn_reader<R: crate::platform::GatePipe + Send + 'static>(
    name: &'static str,
    mut pipe: R,
    stop: Arc<AtomicBool>,
) -> io::Result<Reader> {
    crate::platform::gate_pipe_nonblocking(&pipe)?;
    let (tx, rx) = mpsc::channel();
    let join = std::thread::Builder::new()
        .name(name.into())
        .spawn(move || {
            crate::platform::qos::set_self(crate::platform::qos::Qos::Background);
            let mut tail = ByteTail::default();
            let mut bytes = [0; READ_BYTES];
            let result = loop {
                if stop.load(Ordering::Acquire) {
                    break Ok(());
                }
                match crate::platform::gate_pipe_read(&mut pipe, &mut bytes) {
                    Ok(0) => break Ok(()),
                    Ok(n) => tail.push(&bytes[..n]),
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) if error.kind() == io::ErrorKind::BrokenPipe => break Ok(()),
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        if stop.load(Ordering::Acquire) {
                            break Ok(());
                        }
                        std::thread::sleep(POLL);
                    }
                    Err(error) => break Err(error),
                }
            };
            let value = result.map(|()| tail.result());
            if tx.send(value).is_err() { /* receiver left after a bounded stop */ }
        })?;
    Ok(Reader { receiver: rx, join })
}

fn merge(out: &TailResult, err: &TailResult) -> String {
    // Preserve the historical `Command::output` presentation: stdout followed
    // by stderr, with no framing when both bounded captures fit. The marker is
    // appended only when a per-stream byte tail actually discarded data.
    let mut log = String::with_capacity(out.text.len() + err.text.len() + 64);
    log.push_str(&out.text);
    log.push_str(&err.text);
    if let Some(marker) = truncation_marker(out.truncated, err.truncated) {
        log.push_str(&marker);
    }
    log
}

fn truncation_marker(stdout: bool, stderr: bool) -> Option<String> {
    match (stdout, stderr) {
        (false, false) => None,
        (true, true) => Some(format!(
            "\n[stdout and stderr tails truncated; showing last {TAIL_BYTES} bytes per stream]"
        )),
        (true, false) => Some(format!(
            "\n[stdout tail truncated; showing last {TAIL_BYTES} bytes]"
        )),
        (false, true) => Some(format!(
            "\n[stderr tail truncated; showing last {TAIL_BYTES} bytes]"
        )),
    }
}

fn poll_reader(rx: &mpsc::Receiver<io::Result<TailResult>>) -> Option<io::Result<TailResult>> {
    match rx.try_recv() {
        Ok(result) => Some(result),
        Err(mpsc::TryRecvError::Disconnected) => {
            Some(Err(io::Error::other("reader thread disconnected")))
        }
        Err(mpsc::TryRecvError::Empty) => None,
    }
}

fn collect_streams(
    out: Reader,
    err: Reader,
    stop: &AtomicBool,
    deadline: Option<Instant>,
    mut out_ready: Option<io::Result<TailResult>>,
    mut err_ready: Option<io::Result<TailResult>>,
) -> Result<StreamResult, (TailResult, TailResult)> {
    let receive = |rx: &mpsc::Receiver<io::Result<TailResult>>,
                   ready: &mut Option<io::Result<TailResult>>| {
        if ready.is_some() {
            return ready.take();
        }
        loop {
            match rx.try_recv() {
                Ok(result) => return Some(result),
                Err(mpsc::TryRecvError::Disconnected) => {
                    return Some(Err(io::Error::other("reader thread disconnected")));
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
            if deadline.is_some_and(|end| Instant::now() >= end) {
                return None;
            }
            std::thread::sleep(POLL);
        }
    };
    let mut out_result = receive(&out.receiver, &mut out_ready);
    let mut err_result = receive(&err.receiver, &mut err_ready);
    if out_result.is_none() || err_result.is_none() {
        stop.store(true, Ordering::Release);
        let _ = out.join.join();
        let _ = err.join.join();
        if out_result.is_none() {
            out_result = out.receiver.try_recv().ok();
        }
        if err_result.is_none() {
            err_result = err.receiver.try_recv().ok();
        }
        return Err((
            out_result.and_then(Result::ok).unwrap_or_default(),
            err_result.and_then(Result::ok).unwrap_or_default(),
        ));
    }
    let out_result = out_result.expect("checked stdout result");
    let err_result = err_result.expect("checked stderr result");
    let out_joined = out.join.join().is_ok();
    let err_joined = err.join.join().is_ok();
    let readers_ok = out_joined && err_joined;
    let stdout = out_result.as_ref().map_or_else(
        |_| empty_tail(),
        |value| TailResult {
            text: value.text.clone(),
            truncated: value.truncated,
        },
    );
    let stderr = err_result.as_ref().map_or_else(
        |_| empty_tail(),
        |value| TailResult {
            text: value.text.clone(),
            truncated: value.truncated,
        },
    );
    let reader_error = (!readers_ok).then(|| io::Error::other("reader thread panicked"));
    Ok(StreamResult {
        stdout,
        stderr,
        out_result: out_result
            .map(|_| ())
            .and_then(|_| reader_error.map_or(Ok(()), Err)),
        err_result: err_result.map(|_| ()),
    })
}

fn empty_tail() -> TailResult {
    TailResult {
        text: String::new(),
        truncated: false,
    }
}

/// Run a configured gate command with independent fixed-capacity byte tails.
/// The caller retains the `Workspace` lease through this return. A `Poisoned`
/// result means a descendant may still be using the worktree and the caller
/// must quarantine the lease rather than clean it up or reuse it.
pub(super) fn run(mut command: Command, timeout: Duration) -> CaptureResult {
    let deadline = if timeout.is_zero() {
        None
    } else {
        match Instant::now().checked_add(timeout) {
            Some(deadline) => Some(deadline),
            None => {
                return CaptureResult::Infrastructure {
                    reason: "configured gate deadline exceeds the platform clock range".into(),
                    log: String::new(),
                };
            }
        }
    };
    if REAPER_PENDING.load(Ordering::Acquire) != 0 {
        return CaptureResult::Infrastructure {
            reason:
                "a previous gate child still has uncertain reap ownership; gate execution is held"
                    .into(),
            log: String::new(),
        };
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let (mut child, group) = match crate::platform::spawn_gate_grouped(&mut command) {
        Ok(pair) => pair,
        Err(error) => {
            return CaptureResult::Infrastructure {
                reason: format!(
                    "gate command could not be started with process containment: {error}"
                ),
                log: String::new(),
            };
        }
    };
    let stop = Arc::new(AtomicBool::new(false));
    let out = child.stdout.take().expect("stdout was piped");
    let err = child.stderr.take().expect("stderr was piped");
    let out_rx = match spawn_reader("thegn-gate-stdout", out, Arc::clone(&stop)) {
        Ok(rx) => rx,
        Err(error) => return terminate_after_reader_failure(child, group, stop, None, error),
    };
    let err_rx = match spawn_reader("thegn-gate-stderr", err, Arc::clone(&stop)) {
        Ok(rx) => rx,
        Err(error) => {
            return terminate_after_reader_failure(child, group, stop, Some(out_rx), error);
        }
    };
    let mut leader_exited = false;
    let mut wait_error = None;
    let mut timed_out = false;
    let mut out_ready = None;
    let mut err_ready = None;
    loop {
        if out_ready.is_none() {
            out_ready = poll_reader(&out_rx.receiver);
        }
        if err_ready.is_none() {
            err_ready = poll_reader(&err_rx.receiver);
        }
        if out_ready.as_ref().is_some_and(|result| result.is_err())
            || err_ready.as_ref().is_some_and(|result| result.is_err())
        {
            break;
        }
        match crate::platform::gate_child_exited(&mut child) {
            Ok(true) => {
                leader_exited = true;
                break;
            }
            Ok(false) => {}
            Err(error) => {
                wait_error = Some(error);
                break;
            }
        }
        if deadline.is_some_and(|end| Instant::now() >= end) {
            timed_out = true;
            break;
        }
        std::thread::sleep(POLL);
    }
    let reader_failed = out_ready.as_ref().is_some_and(|result| result.is_err())
        || err_ready.as_ref().is_some_and(|result| result.is_err());
    if timed_out || reader_failed || wait_error.is_some() {
        group.terminate();
        // Keep the direct child unreaped during the grace window. That pins
        // its identity while we may still force-kill its process group.
        std::thread::sleep(TERM_GRACE);
        group.kill();
    }
    if !leader_exited {
        let reap_until = Instant::now() + REAP_GRACE;
        while wait_error.is_none() && Instant::now() < reap_until {
            match crate::platform::gate_child_exited(&mut child) {
                Ok(true) => {
                    leader_exited = true;
                    wait_error = None;
                    break;
                }
                Ok(false) => std::thread::sleep(POLL),
                Err(error) => {
                    wait_error = Some(error);
                    break;
                }
            }
        }
        if !leader_exited {
            stop.store(true, Ordering::Release);
            let reason = wait_error.map_or_else(
                || if reader_failed {
                    "gate output reader failed and the process group did not settle after force termination".to_string()
                } else {
                    "gate process group did not settle after force termination".to_string()
                },
                |error| format!("gate child reap failed: {error}"),
            );
            let (stdout, stderr) = collect_streams(
                out_rx,
                err_rx,
                &stop,
                Some(Instant::now() + TERM_GRACE),
                out_ready,
                err_ready,
            )
            .map(|streams| (streams.stdout, streams.stderr))
            .unwrap_or_default();
            let log = merge(&stdout, &stderr);
            if !queue_reaper((child, group)) {
                return CaptureResult::Poisoned {
                    reason: format!("{reason}; reaper unavailable and child ownership retained"),
                    log,
                };
            }
            return CaptureResult::Poisoned { reason, log };
        }
    }
    let streams_deadline = deadline.map(|end| end.max(Instant::now() + TERM_GRACE));
    let streams = match collect_streams(
        out_rx,
        err_rx,
        &stop,
        streams_deadline,
        out_ready,
        err_ready,
    ) {
        Ok(streams) => streams,
        Err((stdout, stderr)) => {
            let mut log = merge(&stdout, &stderr);
            log.push_str("\noutput truncated: a descendant retained a pipe");
            // On Unix the leader remains waitable (WNOWAIT), pinning the process
            // group id until this force-kill is delivered. Windows retains the Job
            // Object handle even if Child::try_wait already observed the leader.
            group.kill();
            #[expect(
                clippy::disallowed_methods,
                reason = "the owned group was force-killed before the observed leader was reaped"
            )]
            if let Err(error) = child.wait() {
                if !queue_reaper((child, group)) {
                    return CaptureResult::Poisoned {
                        reason: format!(
                            "gate child reap failed after output timeout: {error}; ownership retained"
                        ),
                        log,
                    };
                }
                return CaptureResult::Poisoned {
                    reason: format!("gate child reap failed after output timeout: {error}"),
                    log,
                };
            }
            return CaptureResult::Poisoned {
                reason: "gate descendant retained an output pipe after the deadline; the gate workspace lease is quarantined".into(),
                log,
            };
        }
    };
    #[expect(
        clippy::disallowed_methods,
        reason = "direct-child exit and output-reader settlement were observed on the off-loop gate worker"
    )]
    let status = match child.wait() {
        Ok(status) => status,
        Err(error) => {
            let log = merge(&streams.stdout, &streams.stderr);
            if !queue_reaper((child, group)) {
                return CaptureResult::Poisoned {
                    reason: format!("gate child reap failed: {error}; ownership retained"),
                    log,
                };
            }
            return CaptureResult::Poisoned {
                reason: format!("gate child reap failed: {error}"),
                log,
            };
        }
    };
    let mut group_deadline_expired = false;
    while !group.is_empty() {
        if deadline.is_some_and(|end| Instant::now() >= end) {
            group_deadline_expired = true;
            break;
        }
        std::thread::sleep(POLL);
    }
    if group_deadline_expired {
        // The direct child has been reaped, so the process-group id is never
        // used as a signal target here. Quarantine prevents cache/worktree reuse.
        let mut log = merge(&streams.stdout, &streams.stderr);
        log.push_str("\noutput truncated: gate process group outlived its leader");
        return CaptureResult::Poisoned {
            reason: "gate process group remained active after its direct child exited; the workspace lease is quarantined".into(),
            log,
        };
    }
    let log = merge(&streams.stdout, &streams.stderr);
    if let Err(error) = streams.out_result {
        return CaptureResult::Infrastructure {
            reason: format!("gate stdout reader failed: {error}"),
            log,
        };
    }
    if let Err(error) = streams.err_result {
        return CaptureResult::Infrastructure {
            reason: format!("gate stderr reader failed: {error}"),
            log,
        };
    }
    if timed_out {
        CaptureResult::Timeout { log }
    } else {
        CaptureResult::Completed { status, log }
    }
}

/// The log text for a degraded path that captured only one stream.
///
/// Renders through [`merge`] rather than taking `TailResult::text` directly, so
/// the truncation marker follows the same rule everywhere — and so an
/// untruncated tail still renders byte-identically to the historical output.
fn recv_log(reader: Option<Reader>) -> String {
    reader
        .and_then(recv_tail)
        .map(|tail| merge(&tail, &TailResult::default()))
        .unwrap_or_default()
}

fn recv_tail(reader: Reader) -> Option<TailResult> {
    let result = reader
        .receiver
        .recv_timeout(REAP_GRACE)
        .ok()
        .and_then(Result::ok);
    let _ = reader.join.join();
    result
}

fn queue_reaper(owner: ReapOwner) -> bool {
    REAPER_PENDING.fetch_add(1, Ordering::AcqRel);
    let sender = REAPER.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<ReapOwner>();
        std::thread::Builder::new()
            .name("thegn-gate-reaper".into())
            .spawn(move || {
                crate::platform::qos::set_self(crate::platform::qos::Qos::Background);
                while let Ok((mut child, group)) = rx.recv() {
                    #[expect(clippy::disallowed_methods, reason = "the dedicated off-loop gate reaper retains child ownership until the OS reports reap")]
                    let result = child.wait();
                    finish_reap(result, child, group, &REAPER_PENDING);
                }
            })
            .map_err(|error| error.to_string())?;
        Ok(tx)
    });
    match sender {
        Ok(sender) => match sender.send(owner) {
            Ok(()) => true,
            Err(error) => {
                std::mem::forget(error.0);
                false
            }
        },
        Err(_) => {
            // Initialization failed; retain the owner and leave the global
            // pending count set so another gate cannot spawn around it.
            std::mem::forget(owner);
            false
        }
    }
}

fn finish_reap(
    result: io::Result<ExitStatus>,
    child: Child,
    group: crate::platform::GroupHandle,
    pending: &AtomicUsize,
) {
    match result {
        Ok(_) => {
            // `group` is a plain handle with no Drop impl, so there is nothing to
            // release here — the reap itself is what ended the group's life.
            pending.fetch_sub(1, Ordering::AcqRel);
        }
        Err(_) => {
            // Preserve both the process handle and group owner; an unproven
            // wait keeps later gates held. The pending count stays nonzero.
            std::mem::forget((child, group));
        }
    }
}

fn terminate_after_reader_failure(
    mut child: Child,
    group: crate::platform::GroupHandle,
    stop: Arc<AtomicBool>,
    reader: Option<Reader>,
    error: io::Error,
) -> CaptureResult {
    group.terminate();
    std::thread::sleep(TERM_GRACE);
    group.kill();
    stop.store(true, Ordering::Release);
    let deadline = Instant::now() + REAP_GRACE;
    let mut failure = None;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => {
                let log = recv_log(reader);
                return if group.is_empty() {
                    CaptureResult::Infrastructure {
                        reason: format!("gate output reader could not start: {error}"),
                        log,
                    }
                } else {
                    CaptureResult::Poisoned {
                        reason: format!(
                            "gate output reader could not start: {error}; process group remained active after force termination"
                        ),
                        log,
                    }
                };
            }
            Ok(None) if Instant::now() < deadline => std::thread::sleep(POLL),
            Ok(None) => break,
            Err(reap) => {
                failure = Some(reap);
                break;
            }
        }
    }
    let log = recv_log(reader);
    let reason = failure.map_or_else(
        || format!("gate output reader could not start ({error}); child did not reap after force termination"),
        |reap| format!("gate output reader could not start ({error}) and child reap failed ({reap})"),
    );
    if queue_reaper((child, group)) {
        CaptureResult::Poisoned { reason, log }
    } else {
        CaptureResult::Poisoned {
            reason: format!("{reason}; bounded reaper unavailable, process ownership retained"),
            log,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn byte_tail_is_fixed_capacity_and_preserves_split_utf8_bytes() {
        let mut tail = ByteTail::default();
        tail.push(&vec![b'x'; TAIL_BYTES - 1]);
        tail.push(&[0xe2]);
        tail.push(&[0x82, 0xac]);
        assert_eq!(tail.bytes.len(), TAIL_BYTES);
        assert!(tail.result().text.ends_with('€'));
    }

    #[test]
    fn byte_tail_marks_only_actual_clipping() {
        let mut short = ByteTail::default();
        short.push(b"small output");
        assert!(!short.result().truncated);

        let mut exact_capacity = ByteTail::default();
        exact_capacity.push(&vec![b'x'; TAIL_BYTES]);
        assert!(!exact_capacity.result().truncated);

        let mut clipped = ByteTail::default();
        clipped.push(&vec![b'x'; TAIL_BYTES + 1]);
        let result = clipped.result();
        assert_eq!(result.text.len(), TAIL_BYTES);
        assert!(result.truncated);
    }

    #[test]
    fn untruncated_log_keeps_stdout_then_stderr_without_framing() {
        let out = TailResult {
            text: "stdout".into(),
            truncated: false,
        };
        let err = TailResult {
            text: "stderr".into(),
            truncated: false,
        };
        assert_eq!(merge(&out, &err), "stdoutstderr");
    }

    macro_rules! shell {
        ($script:expr) => {
            match crate::platform::gate_test_shell($script) {
                Some(command) => command,
                None => return,
            }
        };
    }

    #[test]
    fn large_dual_stream_capture_keeps_only_bounded_tails() {
        let output = run(
            shell!(
                "head -c 250000 /dev/zero | tr '\\000' o; head -c 250000 /dev/zero | tr '\\000' e >&2"
            ),
            Duration::from_secs(5),
        );
        let CaptureResult::Completed { status, log } = output else {
            panic!("expected completed gate, got {output:?}")
        };
        assert!(status.success());
        assert!(
            log.len() <= 2 * TAIL_BYTES + truncation_marker(true, true).unwrap().len(),
            "diagnostic tail exceeds its fixed bound: {}",
            log.len()
        );
        let expected = format!(
            "{}{}{}",
            "o".repeat(TAIL_BYTES),
            "e".repeat(TAIL_BYTES),
            truncation_marker(true, true).unwrap()
        );
        assert_eq!(log, expected, "both streams must be drained and tailed");
    }

    #[test]
    fn production_runner_times_out_and_kills_a_term_ignoring_writer() {
        let output = run(
            shell!("trap '' TERM; while :; do printf noisy; done"),
            Duration::from_millis(120),
        );
        assert!(matches!(output, CaptureResult::Timeout { .. }));
    }

    #[test]
    fn leader_exit_with_grandchild_pipe_is_infrastructure_poison() {
        let output = run(shell!("(sleep 0.6) & exit 0"), Duration::from_millis(120));
        assert!(
            matches!(output, CaptureResult::Poisoned { reason, .. } if reason.contains("retained an output pipe"))
        );
        // The escaped writer naturally closes the pipe shortly after the test;
        // this fixture tests reader cancellation without signalling a reaped PID.
        std::thread::sleep(Duration::from_millis(650));
    }

    #[test]
    fn leader_exit_with_live_grandchild_is_not_a_pass_even_if_pipes_close() {
        let output = run(
            shell!("(sleep 0.6 >/dev/null 2>&1) & exit 0"),
            Duration::from_millis(120),
        );
        assert!(
            matches!(output, CaptureResult::Poisoned { reason, .. } if reason.contains("process group remained active"))
        );
        std::thread::sleep(Duration::from_millis(500));
    }

    #[test]
    fn timeout_does_not_signal_an_unrelated_process_group() {
        let command = shell!("while :; do sleep 1; done");
        let mut unrelated = shell!("exec sleep 3").spawn().unwrap();
        let output = run(command, Duration::from_millis(100));
        assert!(matches!(output, CaptureResult::Timeout { .. }));
        assert!(
            unrelated.try_wait().unwrap().is_none(),
            "timeout signalled unrelated pid"
        );
        let _ = unrelated.kill();
        #[expect(
            clippy::disallowed_methods,
            reason = "test cleanup reaps the unrelated fixture child"
        )]
        let _ = unrelated.wait();
    }

    #[test]
    fn ordinary_pass_and_red_exit_keep_the_exit_classification_input() {
        let pass = run(shell!("printf pass; exit 0"), Duration::from_secs(2));
        assert!(
            matches!(pass, CaptureResult::Completed { status, log } if status.success() && log.contains("pass"))
        );
        let red = run(shell!("printf red >&2; exit 7"), Duration::from_secs(2));
        assert!(
            matches!(red, CaptureResult::Completed { status, log } if status.code() == Some(7) && log.contains("red"))
        );
    }

    #[test]
    fn spawn_and_reader_failures_are_infrastructure_results() {
        let _ = shell!("true");
        let missing = Command::new("thegn-gate-command-that-does-not-exist");
        assert!(matches!(
            run(missing, Duration::from_secs(1)),
            CaptureResult::Infrastructure { reason, .. } if reason.contains("could not be started")
        ));

        struct FailingReader;
        impl Read for FailingReader {
            fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::other("fixture reader failure"))
            }
        }
        impl crate::platform::GatePipe for FailingReader {
            fn set_nonblocking(&self) -> io::Result<()> {
                Ok(())
            }

            fn read_available(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
                self.read(bytes)
            }
        }
        let reader = FailingReader;
        let stop = Arc::new(AtomicBool::new(false));
        let reader = spawn_reader("thegn-gate-reader-fixture", reader, stop).unwrap();
        assert!(reader.receiver.recv().unwrap().is_err());
        reader.join.join().unwrap();
    }

    #[test]
    fn uncertain_reaper_result_keeps_the_gate_pending_latch_set() {
        let (mut child, group) =
            crate::platform::spawn_gate_grouped(&mut shell!("exit 0")).unwrap();
        while child.try_wait().unwrap().is_none() {
            std::thread::sleep(POLL);
        }
        let pending = AtomicUsize::new(1);
        finish_reap(
            Err(io::Error::other("injected uncertain wait")),
            child,
            group,
            &pending,
        );
        assert_eq!(pending.load(Ordering::Acquire), 1);
    }
}
