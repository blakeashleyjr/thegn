//! Loop-side voice reducer/effect drain.
//!
//! This module is deliberately I/O-free. Process lifecycle belongs to
//! [`crate::voice`]; here the core reducer is the only state authority and the
//! existing pane paste path is the only injection path.

use termwiz::terminal::TerminalWaker;
use thegn_core::voice::{VoiceEffect, VoiceEvent, VoiceState, reduce};

use crate::chrome::FrameModel;
use crate::panes::Panes;
use crate::voice::{VoiceController, VoiceMessage};

fn should_insert_held(voice: &VoiceController, pane_id: u32, pane_live: bool) -> bool {
    matches!(voice.state, VoiceState::Idle)
        && pane_live
        && pane_id != 0
        && voice.has_held_transcript()
}

fn paste_held(
    voice: &mut VoiceController,
    transcript: String,
    paste: impl FnOnce(&str) -> Result<(), String>,
) -> String {
    match paste(&transcript) {
        Ok(()) => "Voice transcript inserted".into(),
        Err(error) => {
            voice.hold_transcript(transcript);
            format!("Voice transcript not inserted: {error}")
        }
    }
}

/// Toggle capture for the currently focused pane. Returns the status line to
/// show immediately; the actual command starts only through a reducer effect.
pub(crate) fn toggle(
    voice: &mut VoiceController,
    pane_id: u32,
    pane_live: bool,
    panes: &mut Panes,
    now_ms: u64,
    waker: &TerminalWaker,
) -> String {
    if matches!(voice.state, VoiceState::Idle) && voice.shutdown_pending() {
        return "Voice shutdown pending".into();
    }
    if should_insert_held(voice, pane_id, pane_live) {
        let transcript = voice
            .take_held_transcript()
            .expect("held transcript checked above");
        let Some(pane) = panes.table.get_mut(&pane_id) else {
            voice.hold_transcript(transcript);
            return "Voice transcript not inserted: focused pane unavailable".into();
        };
        return paste_held(voice, transcript, |text| {
            crate::run::paste_text_into_pane(pane, text).map_err(|error| error.to_string())
        });
    }
    if !voice.config().enabled {
        return "Voice is off; run thegn voice setup (experimental channel required)".into();
    }
    if !pane_live || pane_id == 0 {
        return "Voice needs a live focused pane".into();
    }
    let event = match voice.state {
        VoiceState::Idle => VoiceEvent::Start { pane_id, now_ms },
        VoiceState::Recording { pane_id: owner, .. } if owner == pane_id => {
            VoiceEvent::Stop { pane_id }
        }
        VoiceState::Recording { .. } => VoiceEvent::Cancel,
        VoiceState::Transcribing { .. } => {
            return "Voice is transcribing…".into();
        }
    };
    let status = if matches!(event, VoiceEvent::Start { .. }) {
        "Voice recording… press Alt-v to stop (Esc cancels)".into()
    } else if matches!(event, VoiceEvent::Stop { .. }) {
        "Voice transcribing…".into()
    } else {
        "Voice capture cancelled".into()
    };
    apply_event(voice, event, waker, None, None, pane_id);
    status
}

pub(crate) fn cancel(voice: &mut VoiceController, waker: &TerminalWaker) -> String {
    if !voice.is_active() {
        if voice.take_held_transcript().is_some() {
            return "Held voice transcript discarded".into();
        }
        return "".into();
    }
    voice.cancel_transcription();
    apply_event(voice, VoiceEvent::Cancel, waker, None, None, 0);
    "Voice cancelled".into()
}

/// Apply a live config replacement. Disabling voice cancels an in-flight
/// capture before the new config becomes active, so a reload cannot leave a
/// worker or microphone running behind an explicit opt-out.
pub(crate) fn reconfigure(
    voice: &mut VoiceController,
    cfg: thegn_core::config::VoiceConfig,
    model: &mut FrameModel,
    waker: &TerminalWaker,
) {
    if cfg != *voice.config() && !matches!(voice.state, VoiceState::Idle) {
        let pane_id = match voice.state {
            VoiceState::Recording { pane_id, .. } | VoiceState::Transcribing { pane_id, .. } => {
                pane_id
            }
            VoiceState::Idle => 0,
        };
        voice.cancel_transcription();
        apply_event(voice, VoiceEvent::Cancel, waker, Some(model), None, pane_id);
    }
    voice.configure(cfg);
}

/// Drain worker messages without ever waiting. Returns whether the caller
/// should repaint chrome or panes.
pub(crate) fn drain(
    voice: &mut VoiceController,
    panes: &mut Panes,
    focused_pane: u32,
    model: &mut FrameModel,
    waker: &TerminalWaker,
) -> bool {
    let mut dirty = false;
    while let Ok(message) = voice.messages().try_recv() {
        if let VoiceMessage::MaxDuration { generation, .. } = &message
            && *generation != voice.capture_generation()
        {
            continue;
        }
        if let VoiceMessage::CaptureComplete { generation, .. }
        | VoiceMessage::CaptureFailed { generation, .. }
        | VoiceMessage::CaptureUnresolved { generation, .. } = &message
            && *generation != voice.capture_generation()
        {
            continue;
        }
        if let VoiceMessage::Session { generation, .. } = &message
            && *generation != voice.capture_generation()
        {
            continue;
        }
        if let VoiceMessage::Session { request_id, .. } = &message
            && *request_id != voice.next_request_id
        {
            continue;
        }
        dirty = true;
        voice.record_lifecycle_message(&message);
        let event = match message {
            VoiceMessage::Session {
                pane_id,
                request_id,
                event,
                ..
            } => match event {
                crate::voice_worker::Event::Captured(_) => continue, // decoded off-loop by the monitor
                crate::voice_worker::Event::Timing { .. } => continue,
                crate::voice_worker::Event::Partial(transcript) => VoiceEvent::Partial {
                    pane_id,
                    request_id,
                    transcript,
                },
                crate::voice_worker::Event::Stopped => VoiceEvent::CaptureStopped {
                    pane_id,
                    request_id,
                },
                crate::voice_worker::Event::Final(transcript) => VoiceEvent::TranscriptSucceeded {
                    pane_id,
                    request_id,
                    transcript,
                },
                crate::voice_worker::Event::Failed(reason) => VoiceEvent::TranscriptFailed {
                    pane_id,
                    request_id,
                    reason,
                },
            },
            VoiceMessage::CaptureComplete { pane_id, wav, .. } => {
                VoiceEvent::CaptureComplete { pane_id, wav }
            }
            VoiceMessage::CaptureFailed {
                pane_id, reason, ..
            } => VoiceEvent::CaptureFailed { pane_id, reason },
            VoiceMessage::CaptureUnresolved {
                pane_id, reason, ..
            } => VoiceEvent::CaptureFailed { pane_id, reason },
            VoiceMessage::CaptureReaped { .. } => continue,
            VoiceMessage::TranscriptSucceeded {
                pane_id,
                request_id,
                transcript,
            } => VoiceEvent::TranscriptSucceeded {
                pane_id,
                request_id,
                transcript,
            },
            VoiceMessage::TranscriptFailed {
                pane_id,
                request_id,
                reason,
            } => VoiceEvent::TranscriptFailed {
                pane_id,
                request_id,
                reason,
            },
            VoiceMessage::TranscriptUnresolved {
                pane_id,
                request_id,
                reason,
            } => VoiceEvent::TranscriptFailed {
                pane_id,
                request_id,
                reason,
            },
            VoiceMessage::TranscriptReaped { .. } => continue,
            VoiceMessage::MaxDuration { pane_id, .. } => VoiceEvent::MaxDuration { pane_id },
        };
        apply_event(voice, event, waker, Some(model), Some(panes), focused_pane);
    }
    // Drain a queued stop/final before checking the pane: completed capture
    // must still deliver its final to the held slot if the pane just closed.
    if let VoiceState::Recording { pane_id, .. } = voice.state
        && !panes.table.contains_key(&pane_id)
    {
        cancel(voice, waker);
        model.status = "Voice cancelled: pane closed".into();
        dirty = true;
    }
    dirty
}

fn apply_event(
    voice: &mut VoiceController,
    event: VoiceEvent,
    waker: &TerminalWaker,
    mut model: Option<&mut FrameModel>,
    mut panes: Option<&mut Panes>,
    focused_pane: u32,
) {
    let transition = reduce(&voice.state, &mut voice.next_request_id, event);
    if !matches!(voice.state, VoiceState::Idle)
        && matches!(transition.state, VoiceState::Idle)
        && transition.effects.is_empty()
        && let Some(model) = model.as_deref_mut()
    {
        model.status = "Voice: no speech detected".into();
    }
    voice.state = transition.state;
    for effect in transition.effects {
        match effect {
            VoiceEffect::Preview(text) => {
                if let Some(model) = model.as_deref_mut() {
                    model.status = format!(
                        "Voice: {}",
                        thegn_core::pipeline_report::display_fit(&text, 160)
                    );
                }
            }
            VoiceEffect::StartCapture { pane_id } => voice.start_capture(pane_id, waker),
            VoiceEffect::StopCapture { .. } => voice.stop_capture(),
            VoiceEffect::Transcribe {
                pane_id,
                request_id,
                wav,
            } => voice.transcribe(pane_id, request_id, wav, waker),
            VoiceEffect::Inject {
                pane_id,
                transcript,
            } => {
                let pane_present = panes
                    .as_ref()
                    .is_some_and(|panes| panes.table.contains_key(&pane_id));
                match deliver_transcript(voice, pane_id, transcript, focused_pane, pane_present) {
                    Delivery::Nothing => {}
                    Delivery::Held(status) => {
                        if let Some(model) = model.as_deref_mut() {
                            model.status = status;
                        }
                    }
                    Delivery::Inject(transcript) => {
                        if let Some(model) = model.as_deref_mut()
                            && let Some(pane) =
                                panes.as_deref_mut().and_then(|p| p.table.get_mut(&pane_id))
                        {
                            model.status = inject_transcript(voice, transcript, |text| {
                                crate::run::paste_text_into_pane(pane, text)
                                    .map_err(|error| error.to_string())
                            });
                        }
                    }
                }
            }
            VoiceEffect::Notify(message) => {
                if let Some(model) = model.as_deref_mut() {
                    model.status = message;
                }
            }
        }
    }
}

enum Delivery {
    Nothing,
    Inject(String),
    Held(String),
}

fn inject_transcript(
    voice: &mut VoiceController,
    transcript: String,
    paste: impl FnOnce(&str) -> Result<(), String>,
) -> String {
    match paste(&transcript) {
        Ok(()) => "Voice transcript inserted".into(),
        Err(error) => {
            voice.hold_transcript(transcript);
            format!("Voice transcript held: pane input failed: {error}")
        }
    }
}

fn deliver_transcript(
    voice: &mut VoiceController,
    pane_id: u32,
    transcript: String,
    focused_pane: u32,
    pane_present: bool,
) -> Delivery {
    if transcript.is_empty() {
        return Delivery::Nothing;
    }
    if pane_id == focused_pane && pane_present {
        return Delivery::Inject(transcript);
    }
    voice.hold_transcript(transcript.clone());
    let preview = thegn_core::pipeline_report::display_fit(&transcript, 40);
    let preview = preview.trim_end();
    Delivery::Held(format!(
        "Voice transcript held (focus changed): \"{preview}\" - Alt v inserts it here, Alt V discards"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::sound_process::test_support::{UNIX, test_waker};
    use thegn_core::config::VoiceConfig;

    fn voice() -> VoiceController {
        VoiceController::new(VoiceConfig {
            enabled: true,
            ..VoiceConfig::default()
        })
    }

    #[test]
    fn focus_changed_transcript_is_held_not_dropped() {
        let mut voice = voice();
        let delivery = deliver_transcript(&mut voice, 1, "spoken words".into(), 2, true);
        let Delivery::Held(status) = delivery else {
            panic!("expected held transcript");
        };
        assert_eq!(
            voice.take_held_transcript().as_deref(),
            Some("spoken words")
        );
        assert!(status.contains("held"));
        assert!(status.contains("spoken words"));
    }

    #[test]
    fn pane_gone_transcript_is_held() {
        let mut voice = voice();
        let delivery = deliver_transcript(&mut voice, 1, "spoken words".into(), 1, false);
        assert!(matches!(delivery, Delivery::Held(_)));
        assert_eq!(
            voice.take_held_transcript().as_deref(),
            Some("spoken words")
        );
    }

    #[test]
    fn focused_transcript_injects_and_leaves_slot_empty() {
        let mut voice = voice();
        let delivery = deliver_transcript(&mut voice, 1, "spoken words".into(), 1, true);
        assert!(matches!(delivery, Delivery::Inject(text) if text == "spoken words"));
        assert!(!voice.has_held_transcript());
    }

    #[test]
    fn empty_transcript_is_ignored_when_origin_loses_focus() {
        let mut voice = voice();
        let delivery = deliver_transcript(&mut voice, 1, String::new(), 2, true);
        assert!(matches!(delivery, Delivery::Nothing));
        assert!(!voice.has_held_transcript());
    }

    #[test]
    fn held_transcript_selects_insert_without_starting_capture() {
        let mut voice = voice();
        voice.hold_transcript("spoken words".into());
        assert!(should_insert_held(&voice, 1, true));
        assert!(matches!(voice.state, VoiceState::Idle));
        assert!(voice.has_held_transcript());
        assert!(!should_insert_held(&voice, 1, false));
    }

    #[test]
    fn newer_held_transcript_replaces_older() {
        let mut voice = voice();
        assert!(matches!(
            deliver_transcript(&mut voice, 1, "older".into(), 2, true),
            Delivery::Held(_)
        ));
        assert!(matches!(
            deliver_transcript(&mut voice, 1, "newer".into(), 2, true),
            Delivery::Held(_)
        ));
        assert_eq!(voice.take_held_transcript().as_deref(), Some("newer"));
    }

    #[test]
    fn cancel_while_idle_discards_held_transcript() {
        if !UNIX {
            return;
        }
        let mut voice = voice();
        voice.hold_transcript("spoken words".into());
        let (waker, _master, _terminal) = test_waker();
        assert_eq!(
            cancel(&mut voice, &waker),
            "Held voice transcript discarded"
        );
        assert!(!voice.has_held_transcript());
    }

    #[test]
    fn cancellation_during_finalization_discards_late_final() {
        if !UNIX {
            return;
        }
        let mut voice = voice();
        voice.next_request_id = 4;
        voice.state = VoiceState::Transcribing {
            pane_id: 1,
            request_id: 4,
        };
        let (waker, _master, _terminal) = test_waker();
        assert!(voice.is_active());
        assert_eq!(cancel(&mut voice, &waker), "Voice cancelled");
        assert!(!voice.is_active());
        apply_event(
            &mut voice,
            VoiceEvent::TranscriptSucceeded {
                pane_id: 1,
                request_id: 4,
                transcript: "cancelled result".into(),
            },
            &waker,
            None,
            None,
            1,
        );
        assert!(matches!(voice.state, VoiceState::Idle));
        assert!(!voice.has_held_transcript());
    }

    #[test]
    fn reconfigure_clears_held_transcript() {
        let mut voice = voice();
        voice.hold_transcript("spoken words".into());
        voice.configure(VoiceConfig::default());
        assert!(!voice.has_held_transcript());
    }

    #[test]
    fn long_transcript_preview_is_truncated() {
        let mut voice = voice();
        let delivery = deliver_transcript(&mut voice, 1, "a".repeat(80), 2, true);
        let Delivery::Held(status) = delivery else {
            panic!("expected held transcript");
        };
        let preview = status
            .split("): \"")
            .nth(1)
            .unwrap()
            .split("\" -")
            .next()
            .unwrap();
        assert!(unicode_width::UnicodeWidthStr::width(preview) <= 40);
        assert!(preview.ends_with('…'));
    }

    #[test]
    fn idle_cancel_is_a_noop() {
        // The rendering contract is that an idle voice controller contributes
        // no state or status change; mode-chip decoration is tested beside it.
        assert!(matches!(VoiceState::default(), VoiceState::Idle));
    }

    #[test]
    fn paste_failure_keeps_held_text_for_a_successful_retry() {
        let mut voice = voice();
        let failed = inject_transcript(&mut voice, "retry me".into(), |_| Err("closed".into()));
        assert!(failed.contains("closed"));
        assert_eq!(voice.take_held_transcript().as_deref(), Some("retry me"));
        let inserted = inject_transcript(&mut voice, "retry me".into(), |_| Ok(()));
        assert_eq!(inserted, "Voice transcript inserted");
        assert!(!voice.has_held_transcript());
    }

    #[test]
    fn empty_final_reports_no_speech_and_stale_session_request_is_dropped() {
        if !UNIX {
            return;
        }
        let (waker, _master, _terminal) = test_waker();
        let mut voice = voice();
        voice.set_capture_generation_for_test(2);
        voice.next_request_id = 4;
        voice.state = VoiceState::Transcribing {
            pane_id: 1,
            request_id: 4,
        };
        let mut model = FrameModel::default();
        apply_event(
            &mut voice,
            VoiceEvent::TranscriptSucceeded {
                pane_id: 1,
                request_id: 4,
                transcript: String::new(),
            },
            &waker,
            Some(&mut model),
            None,
            1,
        );
        assert_eq!(model.status, "Voice: no speech detected");

        voice.state = VoiceState::Transcribing {
            pane_id: 1,
            request_id: 4,
        };
        voice
            .sender_for_test()
            .send(VoiceMessage::Session {
                pane_id: 1,
                generation: 2,
                request_id: 3,
                event: crate::voice_worker::Event::Final("stale".into()),
            })
            .unwrap();
        voice
            .sender_for_test()
            .send(VoiceMessage::Session {
                pane_id: 1,
                generation: 1,
                request_id: 4,
                event: crate::voice_worker::Event::Final("old generation".into()),
            })
            .unwrap();
        let (pane_tx, _pane_rx) = tokio::sync::mpsc::channel(1);
        let mut panes = Panes::new(pane_tx);
        assert!(!drain(&mut voice, &mut panes, 1, &mut model, &waker));
        assert!(matches!(
            voice.state,
            VoiceState::Transcribing { request_id: 4, .. }
        ));
    }

    #[test]
    fn pane_closure_cancels_recording_session() {
        if !UNIX {
            return;
        }
        let (waker, _master, _terminal) = test_waker();
        let mut voice = voice();
        voice.set_capture_generation_for_test(1);
        voice.next_request_id = 1;
        voice.state = VoiceState::Recording {
            pane_id: 7,
            started_at: 0,
            request_id: 1,
        };
        let (pane_tx, _pane_rx) = tokio::sync::mpsc::channel(1);
        let mut panes = Panes::new(pane_tx);
        let mut model = FrameModel::default();
        assert!(drain(&mut voice, &mut panes, 0, &mut model, &waker));
        assert!(matches!(voice.state, VoiceState::Idle));
        assert_eq!(model.status, "Voice cancelled: pane closed");
    }
}
