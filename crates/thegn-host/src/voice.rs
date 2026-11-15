//! Host-side voice workers and the persistent recording mode chip.
//!
//! Commands are started only by the loop-side voice handler after an explicit
//! user action. The workers communicate through a bounded-output channel and
//! pulse the terminal waker; no process or blocking operation runs on the loop.

use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread;
use std::time::Duration;

use termwiz::terminal::TerminalWaker;
use thegn_core::config::VoiceConfig;
use thegn_core::voice::VoiceState;
use thegn_proc::{End, GroupState, Leader, Limits, Stream};

const MAX_CAPTURE_BYTES: usize = 32 * 1024 * 1024;
/// Newest capture stderr kept for a failure reason; never fails a capture.
const CAPTURE_STDERR_TAIL: usize = 64 * 1024;

#[derive(Debug)]
pub(crate) enum VoiceMessage {
    Session {
        pane_id: u32,
        generation: u64,
        request_id: u64,
        event: crate::voice_worker::Event,
    },
    CaptureComplete {
        pane_id: u32,
        generation: u64,
        wav: Vec<u8>,
    },
    CaptureFailed {
        pane_id: u32,
        generation: u64,
        reason: String,
    },
    CaptureUnresolved {
        pane_id: u32,
        generation: u64,
        reason: String,
    },
    CaptureReaped {
        generation: u64,
    },
    TranscriptSucceeded {
        pane_id: u32,
        request_id: u64,
        transcript: String,
    },
    TranscriptFailed {
        pane_id: u32,
        request_id: u64,
        reason: String,
    },
    TranscriptUnresolved {
        pane_id: u32,
        request_id: u64,
        reason: String,
    },
    TranscriptReaped {
        request_id: u64,
    },
    MaxDuration {
        pane_id: u32,
        generation: u64,
    },
}

enum CaptureCommand {
    Stop,
    Cancel,
}

/// Runtime owned by the event-loop handler. It contains no lock that the loop
/// needs to take: cancellation is a non-blocking channel send and results are
/// drained with `try_recv`.
pub(crate) struct VoiceController {
    pub(crate) state: VoiceState,
    pub(crate) next_request_id: u64,
    cfg: VoiceConfig,
    tx: Sender<VoiceMessage>,
    rx: Receiver<VoiceMessage>,
    capture_stop: Option<Sender<CaptureCommand>>,
    /// Cancel flag of the in-flight COMMAND capture's `thegn_proc` run. Set
    /// wherever `capture_stop` is taken, dropped or replaced (native captures
    /// keep the channel only).
    capture_stop_flag: Option<Arc<AtomicBool>>,
    capture_worker: Option<thread::JoinHandle<()>>,
    capture_terminal: bool,
    capture_generation: u64,
    transcription_cancel: Option<(u64, Arc<AtomicBool>)>,
    transcription_worker: Option<thread::JoinHandle<()>>,
    transcription_terminal: bool,
    capture_unresolved: Option<u64>,
    capture_reaped: Option<u64>,
    transcription_unresolved: Option<u64>,
    transcription_reaped: Option<u64>,
    held_transcript: Option<String>,
    native_cancel: Option<Arc<AtomicBool>>,
}

impl VoiceController {
    pub(crate) fn new(cfg: VoiceConfig) -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            state: VoiceState::Idle,
            next_request_id: 0,
            cfg,
            tx,
            rx,
            capture_stop: None,
            capture_stop_flag: None,
            capture_worker: None,
            capture_terminal: true,
            capture_generation: 0,
            transcription_cancel: None,
            transcription_worker: None,
            transcription_terminal: true,
            capture_unresolved: None,
            capture_reaped: None,
            transcription_unresolved: None,
            transcription_reaped: None,
            held_transcript: None,
            native_cancel: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn set_capture_generation_for_test(&mut self, generation: u64) {
        self.capture_generation = generation;
    }

    #[cfg(test)]
    pub(crate) fn sender_for_test(&self) -> Sender<VoiceMessage> {
        self.tx.clone()
    }

    pub(crate) fn configure(&mut self, cfg: VoiceConfig) {
        if self.cfg != cfg {
            self.held_transcript = None;
        }
        self.cfg = cfg;
    }

    pub(crate) fn config(&self) -> &VoiceConfig {
        &self.cfg
    }

    pub(crate) fn is_recording(&self) -> bool {
        self.state.is_recording()
    }

    pub(crate) fn is_active(&self) -> bool {
        !matches!(self.state, VoiceState::Idle)
    }

    pub(crate) fn hold_transcript(&mut self, text: String) {
        self.held_transcript = Some(text);
    }

    pub(crate) fn take_held_transcript(&mut self) -> Option<String> {
        self.held_transcript.take()
    }

    pub(crate) fn has_held_transcript(&self) -> bool {
        self.held_transcript.is_some()
    }

    pub(crate) fn shutdown_pending(&self) -> bool {
        self.capture_unresolved.is_some()
            || self.transcription_unresolved.is_some()
            || (self.capture_worker.is_some()
                && !self.capture_terminal
                && self
                    .capture_worker
                    .as_ref()
                    .is_some_and(|worker| !worker.is_finished()))
            || (self.transcription_worker.is_some()
                && !self.transcription_terminal
                && self
                    .transcription_worker
                    .as_ref()
                    .is_some_and(|worker| !worker.is_finished()))
    }

    pub(crate) fn messages(&self) -> &Receiver<VoiceMessage> {
        &self.rx
    }

    pub(crate) fn record_lifecycle_message(&mut self, message: &VoiceMessage) {
        match message {
            VoiceMessage::Session {
                generation,
                event: crate::voice_worker::Event::Final(_) | crate::voice_worker::Event::Failed(_),
                ..
            } if *generation == self.capture_generation => {
                self.capture_terminal = true;
                self.release_capture_stop();
                self.native_cancel = None;
            }
            VoiceMessage::CaptureComplete { generation, .. }
            | VoiceMessage::CaptureFailed { generation, .. }
            | VoiceMessage::CaptureUnresolved { generation, .. }
                if *generation == self.capture_generation =>
            {
                self.capture_finished(
                    *generation,
                    matches!(message, VoiceMessage::CaptureUnresolved { .. }),
                );
                self.capture_terminal = true;
            }
            VoiceMessage::CaptureReaped { generation } => self.capture_reaped(*generation),
            VoiceMessage::TranscriptSucceeded { request_id, .. }
            | VoiceMessage::TranscriptFailed { request_id, .. }
            | VoiceMessage::TranscriptUnresolved { request_id, .. }
                if *request_id == self.next_request_id =>
            {
                self.transcription_finished(
                    *request_id,
                    matches!(message, VoiceMessage::TranscriptUnresolved { .. }),
                );
                self.transcription_terminal = true;
            }
            VoiceMessage::TranscriptReaped { request_id } => self.transcription_reaped(*request_id),
            _ => {}
        }
    }

    pub(crate) fn start_capture(&mut self, pane_id: u32, waker: &TerminalWaker) {
        if self
            .capture_worker
            .as_ref()
            .is_some_and(|worker| !worker.is_finished())
            || self
                .transcription_worker
                .as_ref()
                .is_some_and(|worker| !worker.is_finished())
        {
            tracing::warn!("voice capture start refused while previous worker is still running");
            self.capture_generation = self.capture_generation.saturating_add(1);
            let generation = self.capture_generation;
            self.release_capture_stop();
            self.capture_terminal = true;
            send_capture(
                &self.tx,
                waker,
                VoiceMessage::CaptureFailed {
                    pane_id,
                    generation,
                    reason: "previous voice worker is still shutting down".into(),
                },
            );
            return;
        }
        self.join_finished_workers();
        self.capture_generation = self.capture_generation.saturating_add(1);
        let generation = self.capture_generation;
        let (stop_tx, stop_rx) = mpsc::channel();
        self.release_capture_stop();
        self.capture_stop = Some(stop_tx);
        self.capture_terminal = false;
        if self.cfg.capture_command.is_empty()
            || self.cfg.kind == thegn_core::config::VoiceKind::Embedded
        {
            let cancel = Arc::new(AtomicBool::new(false));
            self.native_cancel = Some(cancel.clone());
            self.capture_worker = spawn_native_session(
                self.cfg.clone(),
                pane_id,
                generation,
                self.next_request_id,
                stop_rx,
                cancel,
                self.tx.clone(),
                waker.clone(),
            );
        } else {
            // The command path observes the cancel flag, not the channel.
            drop(stop_rx);
            let stop_flag = Arc::new(AtomicBool::new(false));
            self.capture_stop_flag = Some(stop_flag.clone());
            self.capture_worker = spawn_capture(
                self.cfg.clone(),
                pane_id,
                generation,
                stop_flag,
                self.tx.clone(),
                waker.clone(),
            );
        }
        if self.capture_worker.is_none() {
            self.release_capture_stop();
            self.capture_terminal = true;
            let _ = self.tx.send(VoiceMessage::CaptureFailed {
                pane_id,
                generation,
                reason: "voice capture worker could not start".into(),
            });
            let _ = waker.wake();
        }
    }

    pub(crate) fn stop_capture(&mut self) {
        if let Some(tx) = self.capture_stop.take() {
            let _ = tx.send(CaptureCommand::Stop);
        }
        self.release_capture_stop();
    }

    /// Drop the stop channel and raise the command capture's cancel flag. A
    /// dropped channel already stops a native capture; the flag is the
    /// equivalent for a command capture under `thegn_proc`.
    fn release_capture_stop(&mut self) {
        self.capture_stop = None;
        if let Some(flag) = self.capture_stop_flag.take() {
            flag.store(true, Ordering::Release);
        }
    }

    /// Signal the current transcriber without waiting on its worker. The
    /// worker observes this flag while the provider owns the blocking child.
    pub(crate) fn cancel_transcription(&mut self) {
        if let Some(cancel) = self.native_cancel.take() {
            cancel.store(true, Ordering::Release);
            if let Some(tx) = self.capture_stop.take() {
                let _ = tx.send(CaptureCommand::Cancel);
            }
            self.release_capture_stop();
        }
        if let Some((_, cancel)) = self.transcription_cancel.take() {
            cancel.store(true, Ordering::Release);
        }
    }

    pub(crate) fn transcription_finished(&mut self, request_id: u64, unresolved: bool) {
        if unresolved && self.transcription_reaped != Some(request_id) {
            self.transcription_unresolved = Some(request_id);
        } else if self.transcription_unresolved == Some(request_id) {
            self.transcription_unresolved = None;
        }
        if self
            .transcription_cancel
            .as_ref()
            .is_some_and(|(current, _)| *current == request_id)
        {
            self.transcription_cancel = None;
        }
        self.join_finished_workers();
    }

    pub(crate) fn transcription_reaped(&mut self, request_id: u64) {
        self.transcription_reaped = Some(request_id);
        if self.transcription_unresolved == Some(request_id) {
            self.transcription_unresolved = None;
        }
    }

    pub(crate) fn capture_finished(&mut self, generation: u64, unresolved: bool) {
        if unresolved && self.capture_reaped != Some(generation) {
            self.capture_unresolved = Some(generation);
        } else if self.capture_unresolved == Some(generation) {
            self.capture_unresolved = None;
        }
        self.release_capture_stop();
        self.join_finished_workers();
    }

    pub(crate) fn capture_reaped(&mut self, generation: u64) {
        self.capture_reaped = Some(generation);
        if self.capture_unresolved == Some(generation) {
            self.capture_unresolved = None;
        }
    }

    pub(crate) fn capture_generation(&self) -> u64 {
        self.capture_generation
    }

    pub(crate) fn transcribe(
        &mut self,
        pane_id: u32,
        request_id: u64,
        wav: Vec<u8>,
        waker: &TerminalWaker,
    ) {
        let cfg = self.cfg.clone();
        let tx = self.tx.clone();
        let wk = waker.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        self.transcription_cancel = Some((request_id, cancel.clone()));
        self.transcription_terminal = false;
        match thegn_core::thread_name::try_spawn_named("thegn-voice-stt", move || {
            crate::platform::qos::set_self(crate::platform::qos::Qos::Utility);
            let reaped_tx = tx.clone();
            let reaped_waker = wk.clone();
            let result = if cfg.kind == thegn_core::config::VoiceKind::Http {
                resolve_credential(&cfg).and_then(|token| {
                    thegn_svc::voice_http::transcribe(&cfg, token.as_deref(), &wav, &cancel)
                })
            } else {
                thegn_svc::voice::CommandVoiceProvider::new(cfg)
                    .transcribe_push_to_talk_cancellable_with_reaper(&wav, &cancel, move || {
                        let _ = reaped_tx.send(VoiceMessage::TranscriptReaped { request_id });
                        let _ = reaped_waker.wake();
                    })
            };
            let msg = match result {
                Ok(transcript) => VoiceMessage::TranscriptSucceeded {
                    pane_id,
                    request_id,
                    transcript,
                },
                Err(thegn_core::voice::VoiceError::UnresolvedProcess(reason)) => {
                    VoiceMessage::TranscriptUnresolved {
                        pane_id,
                        request_id,
                        reason,
                    }
                }
                Err(e) => VoiceMessage::TranscriptFailed {
                    pane_id,
                    request_id,
                    reason: e.to_string(),
                },
            };
            let _ = tx.send(msg); // best-effort: loop may have exited
            let _ = wk.wake(); // best-effort: waker pulse
        }) {
            Ok(worker) => self.transcription_worker = Some(worker),
            Err(_) => {
                self.transcription_cancel = None;
                self.transcription_terminal = true;
                if self
                    .tx
                    .send(VoiceMessage::TranscriptFailed {
                        pane_id,
                        request_id,
                        reason: "voice transcription worker could not start".into(),
                    })
                    .is_err()
                {
                    tracing::debug!(
                        "voice loop closed before transcription-start failure notification"
                    );
                }
                if let Err(error) = waker.wake() {
                    tracing::trace!(%error, "voice transcription-start failure wake failed");
                }
            }
        }
    }

    fn join_finished_workers(&mut self) {
        if self
            .capture_worker
            .as_ref()
            .is_some_and(thread::JoinHandle::is_finished)
            && let Some(worker) = self.capture_worker.take()
            && worker.join().is_err()
        {
            tracing::warn!("voice capture worker panicked");
        }
        if self
            .transcription_worker
            .as_ref()
            .is_some_and(thread::JoinHandle::is_finished)
            && let Some(worker) = self.transcription_worker.take()
            && worker.join().is_err()
        {
            tracing::warn!("voice transcription worker panicked");
        }
    }
}

impl Drop for VoiceController {
    fn drop(&mut self) {
        let native = self.native_cancel.is_some();
        self.stop_capture();
        self.cancel_transcription();
        let deadline =
            std::time::Instant::now() + Duration::from_millis(if native { 250 } else { 10 });
        while std::time::Instant::now() < deadline
            && (self
                .capture_worker
                .as_ref()
                .is_some_and(|worker| !worker.is_finished())
                || self
                    .transcription_worker
                    .as_ref()
                    .is_some_and(|worker| !worker.is_finished()))
        {
            thread::sleep(Duration::from_millis(1));
        }
        self.join_finished_workers();
    }
}

fn spawn_capture(
    cfg: VoiceConfig,
    pane_id: u32,
    generation: u64,
    stop_flag: Arc<AtomicBool>,
    tx: Sender<VoiceMessage>,
    waker: TerminalWaker,
) -> Option<thread::JoinHandle<()>> {
    thegn_core::thread_name::try_spawn_named("thegn-voice-cap", move || {
        crate::platform::qos::set_self(crate::platform::qos::Qos::Utility);
        let Some(program) = cfg.capture_command.first().cloned() else {
            send_capture(
                &tx,
                &waker,
                VoiceMessage::CaptureFailed {
                    pane_id,
                    generation,
                    reason: "no capture command configured".into(),
                },
            );
            return;
        };
        let max_duration = cfg.max_duration();
        let argv = thegn_core::sandbox_cpucap::wrap_background_argv(cfg.capture_command);
        let mut command = Command::new(&argv[0]);
        command
            .args(&argv[1..])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // thegn_proc owns the process group: a stop is a cancel (TERM, then
        // KILL after `term_grace`), `max_duration` is the group timeout, and
        // stdout is capped. Escalation and reaping happen inside `run`.
        let run = thegn_proc::run(
            &mut command,
            &Limits {
                stdout: Stream::HeadOrFail {
                    cap: MAX_CAPTURE_BYTES,
                },
                stderr: Stream::Tail {
                    cap: CAPTURE_STDERR_TAIL,
                },
                term_grace: Duration::from_millis(200),
                kill_settle: Duration::from_millis(200),
                reader_grace: Duration::from_millis(100),
                quiesce_after_leader: Some(Duration::ZERO),
                ..Limits::silent(max_duration)
            },
            &stop_flag,
        );
        let leader = match &run.leader {
            Leader::Reaped(status) => Some(LeaderStatus {
                success: status.success(),
                text: status.to_string(),
            }),
            Leader::ReapFailed(_) | Leader::Detached | Leader::NotSpawned => None,
        };
        if matches!(run.leader, Leader::Reaped(_)) && run.group != GroupState::Survivors {
            send_capture(&tx, &waker, VoiceMessage::CaptureReaped { generation });
        }
        let end = match run.end {
            End::LeaderExited => EndClass::LeaderExited,
            End::Cancelled => EndClass::Stopped,
            End::TimedOut => EndClass::TimedOut,
            End::OutputLimit(which) => EndClass::OutputLimit(which),
            End::Spawn(error) => EndClass::Spawn {
                program,
                error: error.to_string(),
            },
            End::Supervise(error) => EndClass::Supervise(error.to_string()),
        };
        let outcome = capture_outcome(end, run.group, leader, run.stdout.bytes, &run.stderr.bytes);
        if outcome.max_duration {
            send_capture(
                &tx,
                &waker,
                VoiceMessage::MaxDuration {
                    pane_id,
                    generation,
                },
            );
        }
        let message = match outcome.result {
            CaptureResult::Complete(wav) => VoiceMessage::CaptureComplete {
                pane_id,
                generation,
                wav,
            },
            CaptureResult::Failed { reason, unresolved } => {
                capture_failure(pane_id, generation, reason, unresolved)
            }
        };
        send_capture(&tx, &waker, message);
    })
    .ok()
}

/// How a command capture's `thegn_proc` run ended, reduced to what the
/// outcome mapping needs (pure, so the matrix is unit-testable).
#[derive(Debug)]
enum EndClass {
    LeaderExited,
    /// The stop flag was seen (stop, cancel, or the controller dropped the capture).
    Stopped,
    /// `max_duration` elapsed.
    TimedOut,
    OutputLimit(thegn_proc::Which),
    Spawn {
        program: String,
        error: String,
    },
    Supervise(String),
}

/// The reaped leader's exit status.
#[derive(Debug)]
struct LeaderStatus {
    success: bool,
    text: String,
}

#[derive(Debug, PartialEq, Eq)]
enum CaptureResult {
    Complete(Vec<u8>),
    Failed { reason: String, unresolved: bool },
}

#[derive(Debug, PartialEq, Eq)]
struct CaptureOutcome {
    /// Send `VoiceMessage::MaxDuration` before the result.
    max_duration: bool,
    result: CaptureResult,
}

fn capture_outcome(
    end: EndClass,
    group: GroupState,
    leader: Option<LeaderStatus>,
    wav: Vec<u8>,
    stderr: &[u8],
) -> CaptureOutcome {
    let failed = |reason: String, unresolved: bool| CaptureResult::Failed { reason, unresolved };
    let max_duration = matches!(end, EndClass::TimedOut);
    let requested_stop = !matches!(end, EndClass::LeaderExited);
    let result = match end {
        EndClass::Spawn { program, error } => failed(format!("start {program}: {error}"), false),
        EndClass::Supervise(error) => failed(format!("wait for capture: {error}"), true),
        // Fail closed: a member that outlived SIGKILL may still hold the device.
        _ if group == GroupState::Survivors => {
            failed("capture process shutdown could not be proven".into(), true)
        }
        EndClass::OutputLimit(thegn_proc::Which::Stdout) => failed(
            format!("command output exceeds {MAX_CAPTURE_BYTES} byte limit"),
            false,
        ),
        EndClass::OutputLimit(thegn_proc::Which::Stderr) => failed(
            format!("capture stderr exceeds {CAPTURE_STDERR_TAIL} byte limit"),
            false,
        ),
        EndClass::LeaderExited | EndClass::Stopped | EndClass::TimedOut => {
            let success = leader.as_ref().is_some_and(|status| status.success);
            if !success && (!requested_stop || wav.is_empty()) {
                let detail = String::from_utf8_lossy(stderr).trim().to_string();
                let reason = if !detail.is_empty() {
                    detail
                } else if let Some(status) = &leader {
                    format!("capture exited with {}", status.text)
                } else {
                    "capture exited without a reapable status".into()
                };
                failed(reason, false)
            } else {
                CaptureResult::Complete(wav)
            }
        }
    };
    CaptureOutcome {
        max_duration,
        result,
    }
}

#[expect(
    clippy::disallowed_methods,
    reason = "the off-loop capture worker must reap its child after termination"
)]
fn terminate_capture_child(
    mut child: std::process::Child,
    group: crate::platform::GroupHandle,
    on_reaped: &Arc<dyn Fn() + Send + Sync>,
) -> bool {
    group.kill();
    if let Err(kill_error) = child.kill() {
        match child.try_wait() {
            Ok(Some(_)) => {}
            Ok(None) => tracing::warn!(%kill_error, "voice capture child kill failed"),
            Err(wait_error) => {
                tracing::warn!(%kill_error, %wait_error, "voice capture child kill and wait failed")
            }
        }
    }
    let until = std::time::Instant::now() + Duration::from_millis(60);
    loop {
        match child.try_wait() {
            Ok(Some(_)) if group.is_empty() => {
                on_reaped();
                return true;
            }
            Ok(Some(_)) if std::time::Instant::now() < until => {
                group.kill();
                thread::sleep(Duration::from_millis(2));
            }
            Ok(None) if std::time::Instant::now() < until => {
                thread::sleep(Duration::from_millis(2))
            }
            Ok(Some(_)) | Ok(None) | Err(_) => break,
        }
    }
    let reaped = Arc::clone(on_reaped);
    let reaped_group = group.clone();
    match thegn_core::thread_name::try_spawn_named("thegn-voice-rp", move || {
        crate::platform::qos::set_self(crate::platform::qos::Qos::Background);
        if let Err(error) = child.wait() {
            tracing::warn!(%error, "voice capture reaper failed");
        } else {
            while !reaped_group.is_empty() {
                reaped_group.kill();
                thread::sleep(Duration::from_millis(2));
            }
            reaped();
        }
    }) {
        Ok(_) => false,
        Err(error) => {
            // The thread could not accept reaper ownership; returning false
            // becomes CaptureUnresolved, and dropping Child here is the
            // explicitly degraded last resort rather than an unbounded wait.
            tracing::error!(%error, "voice capture child could not be handed to reaper");
            false
        }
    }
}

fn join_voice_workers(workers: Vec<thread::JoinHandle<()>>) -> bool {
    let until = std::time::Instant::now() + Duration::from_millis(60);
    while std::time::Instant::now() < until && !workers.iter().all(thread::JoinHandle::is_finished)
    {
        thread::sleep(Duration::from_millis(2));
    }
    if !workers.iter().all(thread::JoinHandle::is_finished) {
        return false;
    }
    // Join every reader, even when an earlier one panicked.
    let mut joined = true;
    for worker in workers {
        joined &= worker.join().is_ok();
    }
    joined
}

fn read_worker_event(
    reader: &mut impl std::io::BufRead,
) -> Result<Option<crate::voice_worker::Event>, String> {
    use crate::voice_worker::{Event, read_line};
    match read_line(reader) {
        Ok(Some(line)) => serde_json::from_str::<Event>(&line)
            .map(Some)
            .map_err(|_| "invalid voice worker event".into()),
        Ok(None) => Ok(None),
        Err(error) => Err(error.to_string()),
    }
}

fn terminal_worker_event(event: &crate::voice_worker::Event) -> Option<Result<(), String>> {
    match event {
        crate::voice_worker::Event::Failed(reason) => Some(Err(reason.clone())),
        crate::voice_worker::Event::Final(_) => Some(Ok(())),
        _ => None,
    }
}

fn worker_eof_before_final() -> String {
    "voice worker exited without a final transcript".into()
}

fn send_capture(tx: &Sender<VoiceMessage>, waker: &TerminalWaker, msg: VoiceMessage) {
    let _ = tx.send(msg); // best-effort: loop may have exited
    let _ = waker.wake(); // best-effort: waker pulse
}

fn capture_failure(
    pane_id: u32,
    generation: u64,
    reason: impl Into<String>,
    unresolved: bool,
) -> VoiceMessage {
    if unresolved {
        VoiceMessage::CaptureUnresolved {
            pane_id,
            generation,
            reason: reason.into(),
        }
    } else {
        VoiceMessage::CaptureFailed {
            pane_id,
            generation,
            reason: reason.into(),
        }
    }
}

/// Return the ordinary mode chip plus a capability-safe recording marker.
pub(crate) fn mode_chip(mode: crate::keymap::Mode, full: bool, recording: bool) -> String {
    let base = if full {
        match mode {
            crate::keymap::Mode::Normal => "NORMAL",
            crate::keymap::Mode::VimNormal => "VIM NORMAL",
            crate::keymap::Mode::VimInsert => "VIM INSERT",
            crate::keymap::Mode::Emacs => "EMACS",
        }
    } else {
        match mode {
            crate::keymap::Mode::Normal => "N",
            crate::keymap::Mode::VimNormal => "V",
            crate::keymap::Mode::VimInsert => "I",
            crate::keymap::Mode::Emacs => "E",
        }
    };
    if recording {
        format!(
            "{} {base}",
            crate::caps::glyph(crate::caps::Glyph::DotFilled)
        )
    } else {
        base.into()
    }
}

fn resolve_credential(cfg: &VoiceConfig) -> Result<Option<String>, thegn_core::voice::VoiceError> {
    match cfg.credential.as_deref() {
        None => Ok(None),
        Some(reference) => crate::secret::resolve_for(reference, "voice")
            .map(Some)
            .ok_or_else(|| {
                thegn_core::voice::VoiceError::Failed(
                    "HTTP credential reference could not be resolved".into(),
                )
            }),
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "worker captures utterance identity and owned lifecycle channels"
)]
fn spawn_native_session(
    cfg: VoiceConfig,
    pane_id: u32,
    generation: u64,
    request_id: u64,
    stop_rx: Receiver<CaptureCommand>,
    cancel: Arc<AtomicBool>,
    tx: Sender<VoiceMessage>,
    waker: TerminalWaker,
) -> Option<thread::JoinHandle<()>> {
    thegn_core::thread_name::try_spawn_named("thegn-voice", move || {
        crate::platform::qos::set_self(crate::platform::qos::Qos::Utility);
        let result = native_session(
            &cfg, pane_id, generation, request_id, stop_rx, cancel, &tx, &waker,
        );
        if let Err(reason) = result {
            send_capture(
                &tx,
                &waker,
                VoiceMessage::Session {
                    pane_id,
                    generation,
                    request_id,
                    event: crate::voice_worker::Event::Failed(reason),
                },
            );
        }
    })
    .ok()
}

#[expect(clippy::too_many_arguments, reason = "private worker protocol monitor")]
fn native_session(
    cfg: &VoiceConfig,
    pane_id: u32,
    generation: u64,
    request_id: u64,
    stop_rx: Receiver<CaptureCommand>,
    cancel: Arc<AtomicBool>,
    tx: &Sender<VoiceMessage>,
    waker: &TerminalWaker,
) -> Result<(), String> {
    use crate::voice_worker::{Event, Request};
    use std::io::BufReader;
    if !cfg!(feature = "voice-native") {
        return Err("native capture is not compiled; use a voice-native/voice-metal build or configure capture_command".into());
    }
    if cancel.load(Ordering::Acquire) {
        return Ok(());
    }
    if cfg.kind == thegn_core::config::VoiceKind::Http {
        cfg.validate_credential_transport()?;
    }
    let token = if cfg.kind == thegn_core::config::VoiceKind::Http {
        resolve_credential(cfg).map_err(|e| e.to_string())?
    } else {
        None
    };
    if cancel.load(Ordering::Acquire) {
        return Ok(());
    }
    let executable = std::env::current_exe().map_err(|e| e.to_string())?;
    let argv = thegn_core::sandbox_cpucap::wrap_background_argv(vec![
        executable.to_string_lossy().into_owned(),
        "--voice-worker".into(),
    ]);
    let mut command = Command::new(argv[0].as_str());
    command
        .args(&argv[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let (mut child, group) =
        crate::platform::spawn_grouped_best_effort(&mut command).map_err(|e| e.to_string())?;
    let tx_reaped = tx.clone();
    let wk_reaped = waker.clone();
    let on_reaped: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
        send_capture(
            &tx_reaped,
            &wk_reaped,
            VoiceMessage::CaptureReaped { generation },
        )
    });
    let mut output_reader = None;
    let outcome = (|| {
        let mut input = child.stdin.take().ok_or("voice worker input unavailable")?;
        serde_json::to_writer(
            &mut input,
            &Request {
                cfg: cfg.clone(),
                token,
            },
        )
        .map_err(|_| "voice worker request failed")?;
        input
            .write_all(b"\n")
            .map_err(|_| "voice worker request interrupted")?;
        let stdout = child
            .stdout
            .take()
            .ok_or("voice worker output unavailable")?;
        let (event_tx, event_rx) = mpsc::sync_channel(2);
        output_reader = Some(
            thegn_core::thread_name::try_spawn_named("thegn-voice-out", move || {
                crate::platform::qos::set_self(crate::platform::qos::Qos::Utility);
                let mut output = BufReader::new(stdout);
                loop {
                    let event = match read_worker_event(&mut output) {
                        Ok(Some(event)) => Ok(event),
                        Ok(None) => break,
                        Err(error) => Err(error),
                    };
                    let failed = event.is_err();
                    if event_tx.send(event).is_err() || failed {
                        break;
                    }
                }
            })
            .map_err(|e| e.to_string())?,
        );
        let deadline = std::time::Instant::now()
            + cfg.max_duration()
            + cfg.request_timeout()
            + Duration::from_secs(10);
        let monitored = monitor_worker_events(
            event_rx,
            stop_rx,
            &cancel,
            &mut input,
            deadline,
            cfg.request_timeout() + Duration::from_secs(1),
        );
        let mut result = monitored.result;
        for event in monitored.events {
            match event {
                Event::Failed(_) => {}
                Event::Captured(encoded) => {
                    use base64::Engine;
                    if cfg.kind != thegn_core::config::VoiceKind::Command {
                        result = Err("unexpected capture from transcription worker".into());
                    } else {
                        match base64::engine::general_purpose::STANDARD.decode(encoded) {
                            Ok(wav) if wav.len() <= MAX_CAPTURE_BYTES => send_capture(
                                tx,
                                waker,
                                VoiceMessage::CaptureComplete {
                                    pane_id,
                                    generation,
                                    wav,
                                },
                            ),
                            _ => result = Err("invalid native capture payload".into()),
                        }
                    }
                }
                Event::Timing {
                    final_pass,
                    decode_ms,
                    audio_end_ms,
                } => {
                    tracing::info!(target: "thegn::voice", request_id, final_pass, decode_ms, audio_end_ms, "dictation timing");
                }
                event => send_capture(
                    tx,
                    waker,
                    VoiceMessage::Session {
                        pane_id,
                        generation,
                        request_id,
                        event,
                    },
                ),
            }
        }
        drop(input);
        result
    })();
    let reaped = terminate_capture_child(child, group, &on_reaped);
    // Killing the worker closes stdout. Settle its reader with the same bounded
    // join used by legacy capture, entirely away from the UI thread.
    let readers_stopped = output_reader.is_none_or(|reader| join_voice_workers(vec![reader]));
    if !reaped || !readers_stopped {
        send_capture(
            tx,
            waker,
            VoiceMessage::CaptureUnresolved {
                pane_id,
                generation,
                reason: "native voice worker shutdown pending".into(),
            },
        );
    }
    outcome
}

struct WorkerMonitorResult {
    result: Result<(), String>,
    events: Vec<crate::voice_worker::Event>,
}

fn monitor_worker_events(
    event_rx: Receiver<Result<crate::voice_worker::Event, String>>,
    stop_rx: Receiver<CaptureCommand>,
    cancel: &AtomicBool,
    input: &mut impl std::io::Write,
    mut deadline: std::time::Instant,
    stop_timeout: Duration,
) -> WorkerMonitorResult {
    use crate::voice_worker::Event;

    let mut stop_sent = false;
    let mut events = Vec::new();
    let result = loop {
        if cancel.load(Ordering::Acquire) {
            break Ok(());
        }
        match stop_rx.try_recv() {
            Ok(CaptureCommand::Cancel) => break Ok(()),
            Err(TryRecvError::Disconnected) if !stop_sent => break Ok(()),
            Ok(CaptureCommand::Stop) if !stop_sent => {
                if input.write_all(b"stop\n").is_err() {
                    break Err("voice worker stop failed".into());
                }
                stop_sent = true;
                deadline = std::time::Instant::now() + stop_timeout;
            }
            _ => {}
        }
        if std::time::Instant::now() >= deadline {
            break Err("voice worker timed out".into());
        }
        match event_rx.recv_timeout(Duration::from_millis(10)) {
            Ok(Ok(event)) => {
                let terminal = matches!(
                    &event,
                    Event::Final(_) | Event::Failed(_) | Event::Captured(_)
                );
                events.push(event);
                if let Some(Err(reason)) =
                    terminal_worker_event(events.last().expect("just pushed event"))
                {
                    break Err(reason);
                }
                if terminal {
                    break Ok(());
                }
            }
            Ok(Err(reason)) => break Err(reason),
            Err(mpsc::RecvTimeoutError::Disconnected) => break Err(worker_eof_before_final()),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    };
    WorkerMonitorResult { result, events }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::sound_process::test_support::{UNIX, pid_gone, proc_state, test_waker};

    #[test]
    fn idle_chip_is_unchanged_and_recording_chip_is_ascii_safe() {
        let idle = mode_chip(crate::keymap::Mode::Normal, false, false);
        assert_eq!(idle, "N");
        let recording = mode_chip(crate::keymap::Mode::Normal, false, true);
        assert!(recording.ends_with(" N"));
        assert!(!recording.is_empty());
    }

    #[test]
    fn injected_worker_reader_covers_failed_eof_and_oversize_events() {
        let mut failed = std::io::Cursor::new(b"{\"Failed\":\"model unavailable\"}\n".to_vec());
        let event = read_worker_event(&mut failed).unwrap().unwrap();
        assert!(
            matches!(terminal_worker_event(&event), Some(Err(reason)) if reason == "model unavailable")
        );

        let mut eof = std::io::Cursor::new(Vec::<u8>::new());
        assert!(
            read_worker_event(&mut eof).unwrap().is_none(),
            "EOF before Final is observable"
        );

        let mut oversized =
            std::io::Cursor::new(vec![b'x'; crate::voice_worker::MAX_EVENT as usize + 1]);
        assert!(
            read_worker_event(&mut oversized)
                .unwrap_err()
                .contains("protocol limit")
        );
    }

    #[test]
    fn worker_monitor_maps_failed_eof_protocol_errors_and_final_events() {
        use crate::voice_worker::Event;

        let run = |messages: Vec<Result<Event, String>>, close: bool| {
            let (event_tx, event_rx) = mpsc::channel();
            let (stop_tx, stop_rx) = mpsc::channel();
            for message in messages {
                event_tx.send(message).unwrap();
            }
            if close {
                drop(event_tx);
            }
            let mut input = Vec::new();
            let result = monitor_worker_events(
                event_rx,
                stop_rx,
                &AtomicBool::new(false),
                &mut input,
                std::time::Instant::now() + Duration::from_secs(30),
                Duration::from_secs(30),
            );
            drop(stop_tx);
            result
        };

        let failed = run(vec![Ok(Event::Failed("model unavailable".into()))], false);
        assert_eq!(failed.result.unwrap_err(), "model unavailable");

        let eof = run(Vec::new(), true);
        assert!(
            eof.result
                .unwrap_err()
                .contains("without a final transcript")
        );

        let protocol_error = run(vec![Err("protocol limit".into())], false);
        assert_eq!(protocol_error.result.unwrap_err(), "protocol limit");

        let final_event = run(vec![Ok(Event::Final("recognized".into()))], false);
        assert!(final_event.result.is_ok());
        assert!(
            matches!(final_event.events.as_slice(), [Event::Final(text)] if text == "recognized")
        );
    }

    fn reaped(success: bool) -> Option<LeaderStatus> {
        Some(LeaderStatus {
            success,
            text: if success {
                "exit status: 0".into()
            } else {
                "signal: 15 (SIGTERM)".into()
            },
        })
    }

    fn failed(reason: &str, unresolved: bool) -> CaptureResult {
        CaptureResult::Failed {
            reason: reason.into(),
            unresolved,
        }
    }

    #[test]
    fn capture_outcome_maps_every_run_end() {
        let wav = || b"RIFF".to_vec();
        // Leader exits successfully: complete.
        let success = capture_outcome(
            EndClass::LeaderExited,
            GroupState::Drained,
            reaped(true),
            wav(),
            b"",
        );
        assert_eq!(success.result, CaptureResult::Complete(wav()));
        assert!(!success.max_duration);

        // Non-success without a stop: fail with stderr, else the status.
        let with_detail = capture_outcome(
            EndClass::LeaderExited,
            GroupState::Drained,
            reaped(false),
            wav(),
            b"  no microphone\n",
        );
        assert_eq!(with_detail.result, failed("no microphone", false));
        let with_status = capture_outcome(
            EndClass::LeaderExited,
            GroupState::Drained,
            reaped(false),
            wav(),
            b"",
        );
        assert_eq!(
            with_status.result,
            failed("capture exited with signal: 15 (SIGTERM)", false)
        );

        // Stop with audio keeps the audio even though the leader was signalled.
        let stop_with_wav = capture_outcome(
            EndClass::Stopped,
            GroupState::Terminated,
            reaped(false),
            wav(),
            b"",
        );
        assert_eq!(stop_with_wav.result, CaptureResult::Complete(wav()));
        assert!(!stop_with_wav.max_duration);

        // Stop without audio fails.
        let stop_without_wav = capture_outcome(
            EndClass::Stopped,
            GroupState::Terminated,
            reaped(false),
            Vec::new(),
            b"interrupted",
        );
        assert_eq!(stop_without_wav.result, failed("interrupted", false));

        // Timeout announces MaxDuration first, then behaves as a stop.
        let timeout = capture_outcome(
            EndClass::TimedOut,
            GroupState::Terminated,
            reaped(false),
            wav(),
            b"",
        );
        assert!(timeout.max_duration);
        assert_eq!(timeout.result, CaptureResult::Complete(wav()));

        // Oversized stdout keeps the historical wording.
        let limit = capture_outcome(
            EndClass::OutputLimit(thegn_proc::Which::Stdout),
            GroupState::Terminated,
            reaped(false),
            Vec::new(),
            b"",
        );
        assert_eq!(
            limit.result,
            failed(
                &format!("command output exceeds {MAX_CAPTURE_BYTES} byte limit"),
                false
            )
        );

        // Survivors fail closed as unresolved, whatever else happened.
        let survivors = capture_outcome(
            EndClass::LeaderExited,
            GroupState::Survivors,
            reaped(true),
            wav(),
            b"",
        );
        assert_eq!(
            survivors.result,
            failed("capture process shutdown could not be proven", true)
        );

        // Spawn failure keeps the `start {program}` wording.
        let spawn = capture_outcome(
            EndClass::Spawn {
                program: "arecord".into(),
                error: "No such file or directory (os error 2)".into(),
            },
            GroupState::Unknown,
            None,
            Vec::new(),
            b"",
        );
        assert_eq!(
            spawn.result,
            failed(
                "start arecord: No such file or directory (os error 2)",
                false
            )
        );

        // A supervision failure could not prove shutdown.
        let supervise = capture_outcome(
            EndClass::Supervise("waitid failed".into()),
            GroupState::Unknown,
            None,
            Vec::new(),
            b"",
        );
        assert_eq!(
            supervise.result,
            failed("wait for capture: waitid failed", true)
        );
    }

    #[test]
    fn stop_capture_sets_the_capture_flag() {
        let mut controller = VoiceController::new(VoiceConfig::default());
        let flag = Arc::new(AtomicBool::new(false));
        let (stop_tx, _stop_rx) = mpsc::channel();
        controller.capture_stop = Some(stop_tx);
        controller.capture_stop_flag = Some(flag.clone());
        controller.stop_capture();
        assert!(flag.load(Ordering::Acquire));
        assert!(controller.capture_stop.is_none());
        assert!(controller.capture_stop_flag.is_none());

        // Finishing (or replacing) a capture also releases its flag.
        let finished = Arc::new(AtomicBool::new(false));
        controller.capture_stop_flag = Some(finished.clone());
        controller.capture_finished(1, false);
        assert!(finished.load(Ordering::Acquire));
    }

    #[test]
    fn command_capture_returns_wav_on_stop() {
        if !UNIX {
            return;
        }
        let config = VoiceConfig {
            capture_command: vec!["sh".into(), "-c".into(), "printf RIFF; sleep 30".into()],
            ..VoiceConfig::default()
        };
        let (waker, _master, _terminal) = test_waker();
        let (tx, rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let started = std::time::Instant::now();
        let worker = spawn_capture(config, 1, 1, stop.clone(), tx, waker).unwrap();
        thread::sleep(Duration::from_millis(200));
        stop.store(true, Ordering::Release);
        let terminal = loop {
            match rx.recv_timeout(Duration::from_secs(3)).unwrap() {
                VoiceMessage::CaptureReaped { generation } => assert_eq!(generation, 1),
                message => break message,
            }
        };
        match terminal {
            VoiceMessage::CaptureComplete {
                generation, wav, ..
            } => {
                assert_eq!(generation, 1);
                assert_eq!(wav, b"RIFF");
            }
            other => panic!("expected CaptureComplete, got {other:?}"),
        }
        assert!(started.elapsed() < Duration::from_secs(3));
        worker.join().unwrap();
    }

    #[test]
    fn stalled_capture_reader_does_not_make_join_unbounded() {
        let output = thread::spawn(|| thread::sleep(Duration::from_millis(300)));
        let error = thread::spawn(|| {});
        let start = std::time::Instant::now();
        assert!(!join_voice_workers(vec![output, error]));
        assert!(start.elapsed() < Duration::from_millis(200));
    }

    #[test]
    fn unresolved_shutdown_keeps_voice_visibly_busy() {
        let mut controller = VoiceController::new(VoiceConfig::default());
        assert!(!controller.shutdown_pending());
        controller.capture_generation = 1;
        controller.capture_terminal = false;
        controller.capture_worker =
            Some(thread::spawn(|| thread::sleep(Duration::from_millis(150))));
        assert!(controller.shutdown_pending());
        controller
            .tx
            .send(VoiceMessage::CaptureUnresolved {
                pane_id: 1,
                generation: 1,
                reason: "test unresolved child".into(),
            })
            .unwrap();
        let message = controller.messages().try_recv().unwrap();
        controller.record_lifecycle_message(&message);
        assert!(controller.shutdown_pending());
        controller
            .tx
            .send(VoiceMessage::CaptureReaped { generation: 1 })
            .unwrap();
        let message = controller.messages().try_recv().unwrap();
        controller.record_lifecycle_message(&message);
        assert!(!controller.shutdown_pending());
        assert!(
            controller
                .capture_worker
                .as_ref()
                .is_some_and(|worker| !worker.is_finished())
        );
    }

    #[test]
    fn transcribing_can_be_cancelled_without_marking_it_as_recording() {
        let mut controller = VoiceController::new(VoiceConfig::default());
        controller.state = VoiceState::Transcribing {
            pane_id: 1,
            request_id: 7,
        };
        controller.capture_finished(1, true);
        assert!(!controller.is_recording());
        assert!(controller.is_active());
        assert!(controller.shutdown_pending());
    }

    #[test]
    fn refused_capture_start_sends_a_terminal_failure_for_its_generation() {
        if !UNIX {
            return;
        }
        let (waker, _master, _terminal) = test_waker();
        let mut controller = VoiceController::new(VoiceConfig::default());
        controller.transcription_worker = Some(thread::spawn(|| {
            thread::sleep(Duration::from_millis(150));
        }));

        controller.start_capture(1, &waker);

        match controller.messages().try_recv().unwrap() {
            VoiceMessage::CaptureFailed {
                generation, reason, ..
            } => {
                assert_eq!(generation, controller.capture_generation());
                assert_eq!(reason, "previous voice worker is still shutting down");
            }
            other => panic!("expected CaptureFailed, got {other:?}"),
        }
        assert!(controller.capture_worker.is_none());
        assert!(controller.capture_terminal);
        assert!(controller.capture_stop.is_none());
        controller
            .transcription_worker
            .take()
            .unwrap()
            .join()
            .unwrap();
    }

    #[test]
    fn successful_capture_terminates_a_redirected_group_descendant() {
        if !UNIX {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("pid");
        let config = VoiceConfig {
            capture_command: vec![
                "sh".into(),
                "-c".into(),
                "sleep 30 </dev/null >/dev/null 2>&1 & echo $! >\"$1\"; printf RIFF0000WAVE; exec >&- 2>&-; sleep 0.05".into(),
                "sh".into(),
                pid_file.display().to_string(),
            ],
            ..VoiceConfig::default()
        };
        let (waker, _master, _terminal) = test_waker();
        let (tx, rx) = mpsc::channel();
        let worker =
            spawn_capture(config, 1, 1, Arc::new(AtomicBool::new(false)), tx, waker).unwrap();
        let mut saw_reaped = false;
        let terminal = loop {
            match rx.recv_timeout(Duration::from_secs(2)).unwrap() {
                VoiceMessage::CaptureReaped { generation } => {
                    assert_eq!(generation, 1);
                    saw_reaped = true;
                }
                message @ (VoiceMessage::CaptureComplete { .. }
                | VoiceMessage::CaptureFailed { .. }
                | VoiceMessage::CaptureUnresolved { .. }) => break message,
                other => panic!("expected a capture terminal message, got {other:?}"),
            }
        };
        assert!(saw_reaped);
        assert!(matches!(terminal, VoiceMessage::CaptureComplete { .. }));
        worker.join().unwrap();
        let pid = std::fs::read_to_string(pid_file)
            .unwrap()
            .trim()
            .parse::<i32>()
            .unwrap();
        let started = std::time::Instant::now();
        while !pid_gone(pid) && proc_state(pid) != Some('Z') {
            assert!(
                started.elapsed() < Duration::from_millis(500),
                "descendant {pid} remains live"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }
}
