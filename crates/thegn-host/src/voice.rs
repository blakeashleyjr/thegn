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
    MaxDuration {
        pane_id: u32,
        generation: u64,
    },
}

enum CaptureCommand {
    Stop,
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
    capture_generation: u64,
    transcription_cancel: Option<(u64, Arc<AtomicBool>)>,
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
            capture_generation: 0,
            transcription_cancel: None,
        }
    }

    pub(crate) fn configure(&mut self, cfg: VoiceConfig) {
        self.cfg = cfg;
    }

    pub(crate) fn config(&self) -> &VoiceConfig {
        &self.cfg
    }

    pub(crate) fn is_recording(&self) -> bool {
        self.state.is_recording()
    }

    pub(crate) fn messages(&self) -> &Receiver<VoiceMessage> {
        &self.rx
    }

    pub(crate) fn start_capture(&mut self, pane_id: u32, waker: &TerminalWaker) {
        self.capture_generation = self.capture_generation.saturating_add(1);
        let generation = self.capture_generation;
        let (stop_tx, stop_rx) = mpsc::channel();
        self.capture_stop = Some(stop_tx);
        if !spawn_capture(
            self.cfg.clone(),
            pane_id,
            generation,
            stop_rx,
            self.tx.clone(),
            waker.clone(),
        ) {
            self.capture_stop = None;
            let _ = self.tx.send(VoiceMessage::CaptureFailed {
                pane_id,
                generation,
                reason: "voice capture worker could not start".into(),
            });
            let _ = waker.wake();
            return;
        }
        spawn_max_timer(
            pane_id,
            generation,
            self.cfg.max_duration(),
            self.tx.clone(),
            waker.clone(),
        );
    }

    pub(crate) fn stop_capture(&mut self) {
        if let Some(tx) = self.capture_stop.take() {
            let _ = tx.send(CaptureCommand::Stop);
        }
    }

    /// Signal the current transcriber without waiting on its worker. The
    /// worker observes this flag while the provider owns the blocking child.
    pub(crate) fn cancel_transcription(&mut self) {
        if let Some((_, cancel)) = self.transcription_cancel.take() {
            cancel.store(true, Ordering::Release);
        }
    }

    pub(crate) fn transcription_finished(&mut self, request_id: u64) {
        if self
            .transcription_cancel
            .as_ref()
            .is_some_and(|(current, _)| *current == request_id)
        {
            self.transcription_cancel = None;
        }
    }

    pub(crate) fn capture_finished(&mut self) {
        self.capture_stop = None;
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
        if thread::Builder::new()
            .name("voice-transcribe".into())
            .spawn(move || {
                crate::platform::qos::set_self(crate::platform::qos::Qos::Utility);
                let result = thegn_svc::voice::CommandVoiceProvider::new(cfg)
                    .transcribe_push_to_talk_cancellable(&wav, &cancel);
                let msg = match result {
                    Ok(transcript) => VoiceMessage::TranscriptSucceeded {
                        pane_id,
                        request_id,
                        transcript,
                    },
                    Err(e) => VoiceMessage::TranscriptFailed {
                        pane_id,
                        request_id,
                        reason: e.to_string(),
                    },
                };
                let _ = tx.send(msg); // best-effort: loop may have exited
                let _ = wk.wake(); // best-effort: waker pulse
            })
            .is_err()
        {
            self.transcription_cancel = None;
            let _ = self.tx.send(VoiceMessage::TranscriptFailed {
                pane_id,
                request_id,
                reason: "voice transcription worker could not start".into(),
            });
            let _ = waker.wake();
        }
    }
}

impl Drop for VoiceController {
    fn drop(&mut self) {
        self.stop_capture();
        self.cancel_transcription();
    }
}

fn spawn_max_timer(
    pane_id: u32,
    generation: u64,
    duration: Duration,
    tx: Sender<VoiceMessage>,
    waker: TerminalWaker,
) {
    let _ = thread::Builder::new()
        .name("voice-max-duration".into())
        .spawn(move || {
            crate::platform::qos::set_self(crate::platform::qos::Qos::Background);
            thread::sleep(duration);
            let _ = tx.send(VoiceMessage::MaxDuration {
                pane_id,
                generation,
            });
            let _ = waker.wake(); // best-effort: waker pulse
        });
}

fn spawn_capture(
    cfg: VoiceConfig,
    pane_id: u32,
    generation: u64,
    stop_rx: Receiver<CaptureCommand>,
    tx: Sender<VoiceMessage>,
    waker: TerminalWaker,
) -> bool {
    thread::Builder::new()
        .name("voice-capture".into())
        .spawn(move || {
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
            let (mut child, group) = match crate::platform::spawn_grouped(&mut command) {
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
            let output_thread = thread::spawn(move || {
                let _ = output_tx.send(read_capped(stdout, MAX_CAPTURE_BYTES));
            });
            let (error_tx, error_rx) = mpsc::channel();
            let error_thread = thread::spawn(move || {
                let _ = error_tx.send(read_capped(stderr, MAX_CAPTURE_BYTES));
            });
            let mut requested_stop = false;
            let deadline = std::time::Instant::now() + max_duration;
            let mut output = None;
            let mut error = None;
            let status = loop {
                match stop_rx.try_recv() {
                    Ok(CaptureCommand::Stop) => {
                        requested_stop = true;
                        kill_capture_child(&mut child, &group);
                    }
                    Err(TryRecvError::Disconnected) => {
                        requested_stop = true;
                        kill_capture_child(&mut child, &group);
                    }
                    Err(TryRecvError::Empty) => {}
                }
                if !requested_stop && std::time::Instant::now() >= deadline {
                    requested_stop = true;
                    kill_capture_child(&mut child, &group);
                }
                if output.is_none() {
                    output = output_rx.try_recv().ok();
                    if matches!(&output, Some(Err(_))) {
                        terminate_capture_child(&mut child, &group);
                        let _ = output_thread.join();
                        let _ = error_thread.join();
                        let reason = output
                            .and_then(Result::err)
                            .unwrap_or_else(|| "capture output reader failed".into());
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
                }
                if error.is_none() {
                    error = error_rx.try_recv().ok();
                    if matches!(&error, Some(Err(_))) {
                        terminate_capture_child(&mut child, &group);
                        let _ = output_thread.join();
                        let _ = error_thread.join();
                        let reason = error
                            .and_then(Result::err)
                            .unwrap_or_else(|| "capture error reader failed".into());
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
                }
                match child.try_wait() {
                    Ok(Some(status)) => break status,
                    Ok(None) => thread::sleep(Duration::from_millis(5)),
                    Err(e) => {
                        terminate_capture_child(&mut child, &group);
                        let _ = output_thread.join();
                        let _ = error_thread.join();
                        send_capture(
                            &tx,
                            &waker,
                            VoiceMessage::CaptureFailed {
                                pane_id,
                                generation,
                                reason: format!("wait for capture: {e}"),
                            },
                        );
                        return;
                    }
                }
            };
            // A child can exit while a descendant still owns one of the
            // pipes. Kill the grouped process tree before joining helpers so
            // no reader survives the utterance.
            if output.is_none() || error.is_none() {
                terminate_capture_child(&mut child, &group);
            }
            let output_joined = output_thread.join().is_ok();
            let error_joined = error_thread.join().is_ok();
            if !output_joined || !error_joined {
                send_capture(
                    &tx,
                    &waker,
                    VoiceMessage::CaptureFailed {
                        pane_id,
                        generation,
                        reason: "capture pipe reader panicked".into(),
                    },
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
        .is_ok()
}

#[expect(
    clippy::disallowed_methods,
    reason = "the off-loop capture worker must reap its child after termination"
)]
fn terminate_capture_child(child: &mut std::process::Child, group: &crate::platform::GroupHandle) {
    kill_capture_child(child, group);
    let _ = child.wait(); // best-effort: reap after termination
}

fn kill_capture_child(child: &mut std::process::Child, group: &crate::platform::GroupHandle) {
    group.kill();
    let _ = child.kill(); // best-effort: the child may have exited already
}

fn send_capture(tx: &Sender<VoiceMessage>, waker: &TerminalWaker, msg: VoiceMessage) {
    let _ = tx.send(msg); // best-effort: loop may have exited
    let _ = waker.wake(); // best-effort: waker pulse
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_chip_is_unchanged_and_recording_chip_is_ascii_safe() {
        let idle = mode_chip(crate::keymap::Mode::Normal, false, false);
        assert_eq!(idle, "N");
        let recording = mode_chip(crate::keymap::Mode::Normal, false, true);
        assert!(recording.ends_with(" N"));
        assert!(!recording.is_empty());
    }
}
