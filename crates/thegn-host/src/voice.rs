//! Host-side voice workers and the persistent recording mode chip.
//!
//! Commands are started only by the loop-side voice handler after an explicit
//! user action. The workers communicate through a bounded-output channel and
//! pulse the terminal waker; no process or blocking operation runs on the loop.

use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread;
use std::time::Duration;

use termwiz::terminal::TerminalWaker;
use thegn_core::config::VoiceConfig;
use thegn_core::voice::VoiceState;

const MAX_CAPTURE_BYTES: usize = 32 * 1024 * 1024;

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
                self.capture_stop = None;
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
            self.capture_stop = None;
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
            self.capture_worker = spawn_capture(
                self.cfg.clone(),
                pane_id,
                generation,
                stop_rx,
                self.tx.clone(),
                waker.clone(),
            );
        }
        if self.capture_worker.is_none() {
            self.capture_stop = None;
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
    }

    /// Signal the current transcriber without waiting on its worker. The
    /// worker observes this flag while the provider owns the blocking child.
    pub(crate) fn cancel_transcription(&mut self) {
        if let Some(cancel) = self.native_cancel.take() {
            cancel.store(true, Ordering::Release);
            if let Some(tx) = self.capture_stop.take() {
                let _ = tx.send(CaptureCommand::Cancel);
            }
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
        self.capture_stop = None;
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
        match thread::Builder::new()
            .name("thegn-voice-stt".into())
            .spawn(move || {
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
    stop_rx: Receiver<CaptureCommand>,
    tx: Sender<VoiceMessage>,
    waker: TerminalWaker,
) -> Option<thread::JoinHandle<()>> {
    thread::Builder::new()
        .name("thegn-voice-cap".into())
        .spawn(move || {
            crate::platform::qos::set_self(crate::platform::qos::Qos::Utility);
            let reaped_tx = tx.clone();
            let reaped_waker = waker.clone();
            let on_reaped: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
                send_capture(
                    &reaped_tx,
                    &reaped_waker,
                    VoiceMessage::CaptureReaped { generation },
                );
            });
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
            let (mut child, group) = match crate::platform::spawn_grouped_best_effort(&mut command)
            {
                Ok(children) => children,
                Err(e) => {
                    send_capture(
                        &tx,
                        &waker,
                        VoiceMessage::CaptureFailed {
                            pane_id,
                            generation,
                            reason: format!("start {program}: {e}"),
                        },
                    );
                    return;
                }
            };
            let stdout = child.stdout.take().expect("capture stdout was piped");
            let stderr = child.stderr.take().expect("capture stderr was piped");
            let (output_tx, output_rx) = mpsc::channel();
            let output_thread =
                thegn_core::thread_name::spawn_named("thegn-voice-out", move || {
                    crate::platform::qos::set_self(crate::platform::qos::Qos::Utility);
                    let _ = output_tx.send(read_capped(stdout, MAX_CAPTURE_BYTES));
                });
            let (error_tx, error_rx) = mpsc::channel();
            let error_thread = thegn_core::thread_name::spawn_named("thegn-voice-err", move || {
                crate::platform::qos::set_self(crate::platform::qos::Qos::Utility);
                let _ = error_tx.send(read_capped(stderr, MAX_CAPTURE_BYTES));
            });
            let mut requested_stop = false;
            let mut stop_deadline = None;
            let deadline = std::time::Instant::now() + max_duration;
            let mut output = None;
            let mut error = None;
            let status = loop {
                match stop_rx.try_recv() {
                    Ok(CaptureCommand::Stop | CaptureCommand::Cancel) => {
                        if !requested_stop {
                            requested_stop = true;
                            stop_deadline =
                                Some(std::time::Instant::now() + Duration::from_millis(200));
                            kill_capture_child(&mut child, &group);
                        }
                    }
                    Err(TryRecvError::Disconnected) => {
                        if !requested_stop {
                            requested_stop = true;
                            stop_deadline =
                                Some(std::time::Instant::now() + Duration::from_millis(200));
                            kill_capture_child(&mut child, &group);
                        }
                    }
                    Err(TryRecvError::Empty) => {}
                }
                if !requested_stop && std::time::Instant::now() >= deadline {
                    requested_stop = true;
                    stop_deadline = Some(std::time::Instant::now() + Duration::from_millis(200));
                    send_capture(
                        &tx,
                        &waker,
                        VoiceMessage::MaxDuration {
                            pane_id,
                            generation,
                        },
                    );
                    kill_capture_child(&mut child, &group);
                }
                if stop_deadline.is_some_and(|until| std::time::Instant::now() >= until) {
                    let process_stopped = terminate_capture_child(child, group, &on_reaped);
                    let readers_stopped = join_capture_workers(output_thread, error_thread);
                    send_capture(
                        &tx,
                        &waker,
                        capture_failure(
                            pane_id,
                            generation,
                            if process_stopped && readers_stopped {
                                "capture shutdown did not produce a complete result"
                            } else {
                                "capture process shutdown could not be proven"
                            },
                            !process_stopped || !readers_stopped,
                        ),
                    );
                    return;
                }
                if output.is_none() {
                    output = output_rx.try_recv().ok();
                    if matches!(&output, Some(Err(_))) {
                        let process_stopped = terminate_capture_child(child, group, &on_reaped);
                        let readers_stopped = join_capture_workers(output_thread, error_thread);
                        let reason = output
                            .and_then(Result::err)
                            .unwrap_or_else(|| "capture output reader failed".into());
                        send_capture(
                            &tx,
                            &waker,
                            capture_failure(
                                pane_id,
                                generation,
                                if process_stopped && readers_stopped {
                                    reason
                                } else {
                                    "capture shutdown unresolved after output failure".into()
                                },
                                !process_stopped || !readers_stopped,
                            ),
                        );
                        return;
                    }
                }
                if error.is_none() {
                    error = error_rx.try_recv().ok();
                    if matches!(&error, Some(Err(_))) {
                        let process_stopped = terminate_capture_child(child, group, &on_reaped);
                        let readers_stopped = join_capture_workers(output_thread, error_thread);
                        let reason = error
                            .and_then(Result::err)
                            .unwrap_or_else(|| "capture error reader failed".into());
                        send_capture(
                            &tx,
                            &waker,
                            capture_failure(
                                pane_id,
                                generation,
                                if process_stopped && readers_stopped {
                                    reason
                                } else {
                                    "capture shutdown unresolved after error-pipe failure".into()
                                },
                                !process_stopped || !readers_stopped,
                            ),
                        );
                        return;
                    }
                }
                match child.try_wait() {
                    Ok(Some(status)) => break status,
                    Ok(None) => thread::sleep(Duration::from_millis(5)),
                    Err(e) => {
                        let process_stopped = terminate_capture_child(child, group, &on_reaped);
                        let readers_stopped = join_capture_workers(output_thread, error_thread);
                        send_capture(
                            &tx,
                            &waker,
                            capture_failure(
                                pane_id,
                                generation,
                                if process_stopped && readers_stopped {
                                    format!("wait for capture: {e}")
                                } else {
                                    format!("capture shutdown unresolved after wait failure: {e}")
                                },
                                !process_stopped || !readers_stopped,
                            ),
                        );
                        return;
                    }
                }
            };
            // A child can exit while a descendant still owns one of the
            // pipes. Kill the grouped process tree before joining helpers so
            // no reader survives the utterance.
            if (!group.is_empty() || output.is_none() || error.is_none())
                && !terminate_capture_child(child, group, &on_reaped)
            {
                let readers_stopped = join_capture_workers(output_thread, error_thread);
                send_capture(
                    &tx,
                    &waker,
                    capture_failure(
                        pane_id,
                        generation,
                        if readers_stopped {
                            "capture process shutdown could not be proven"
                        } else {
                            "capture process and pipe-worker shutdown could not be proven"
                        },
                        true,
                    ),
                );
                return;
            }
            if !join_capture_workers(output_thread, error_thread) {
                send_capture(
                    &tx,
                    &waker,
                    capture_failure(
                        pane_id,
                        generation,
                        "capture pipe reader did not settle",
                        true,
                    ),
                );
                return;
            }
            let wav = match output.or_else(|| output_rx.recv().ok()) {
                Some(Ok(wav)) => wav,
                Some(Err(reason)) => {
                    send_capture(
                        &tx,
                        &waker,
                        VoiceMessage::CaptureFailed {
                            pane_id,
                            generation,
                            reason,
                        },
                    );
                    return;
                }
                None => {
                    send_capture(
                        &tx,
                        &waker,
                        VoiceMessage::CaptureFailed {
                            pane_id,
                            generation,
                            reason: "capture output reader did not report".into(),
                        },
                    );
                    return;
                }
            };
            let stderr = error
                .or_else(|| error_rx.recv().ok())
                .and_then(Result::ok)
                .unwrap_or_default();
            if !status.success() && (!requested_stop || wav.is_empty()) {
                let detail = String::from_utf8_lossy(&stderr).trim().to_string();
                send_capture(
                    &tx,
                    &waker,
                    VoiceMessage::CaptureFailed {
                        pane_id,
                        generation,
                        reason: if detail.is_empty() {
                            format!("capture exited with {status}")
                        } else {
                            detail
                        },
                    },
                );
            } else {
                send_capture(
                    &tx,
                    &waker,
                    VoiceMessage::CaptureComplete {
                        pane_id,
                        generation,
                        wav,
                    },
                );
            }
        })
        .ok()
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
    match thread::Builder::new()
        .name("thegn-voice-rp".into())
        .spawn(move || {
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

fn join_capture_workers(output: thread::JoinHandle<()>, error: thread::JoinHandle<()>) -> bool {
    join_voice_workers(vec![output, error])
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

fn kill_capture_child(child: &mut std::process::Child, group: &crate::platform::GroupHandle) {
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

fn read_capped(reader: impl Read, cap: usize) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    reader
        .take(cap as u64 + 1)
        .read_to_end(&mut out)
        .map_err(|e| e.to_string())?;
    if out.len() > cap {
        return Err(format!("command output exceeds {cap} byte limit"));
    }
    Ok(out)
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

    #[test]
    fn stalled_capture_reader_does_not_make_join_unbounded() {
        let output = thread::spawn(|| thread::sleep(Duration::from_millis(300)));
        let error = thread::spawn(|| {});
        let start = std::time::Instant::now();
        assert!(!join_capture_workers(output, error));
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
        let (_stop_tx, stop_rx) = mpsc::channel();
        let worker = spawn_capture(config, 1, 1, stop_rx, tx, waker).unwrap();
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
