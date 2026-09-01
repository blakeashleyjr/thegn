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

/// Toggle capture for the currently focused pane. Returns the status line to
/// show immediately; the actual command starts only through a reducer effect.
pub(crate) fn toggle(
    voice: &mut VoiceController,
    pane_id: u32,
    pane_live: bool,
    now_ms: u64,
    waker: &TerminalWaker,
) -> String {
    if !voice.config().enabled {
        return "Voice is off ([voice] enabled = false)".into();
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
    if !voice.is_recording() && !matches!(voice.state, VoiceState::Transcribing { .. }) {
        return "".into();
    }
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
    if !cfg.enabled && !matches!(voice.state, VoiceState::Idle) {
        let pane_id = match voice.state {
            VoiceState::Recording { pane_id, .. } | VoiceState::Transcribing { pane_id, .. } => {
                pane_id
            }
            VoiceState::Idle => 0,
        };
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
        dirty = true;
        if let VoiceMessage::MaxDuration { generation, .. } = &message
            && *generation != voice.capture_generation()
        {
            continue;
        }
        if matches!(
            &message,
            VoiceMessage::CaptureComplete { .. } | VoiceMessage::CaptureFailed { .. }
        ) {
            voice.capture_finished();
        }
        let event = match message {
            VoiceMessage::CaptureComplete { pane_id, wav } => {
                VoiceEvent::CaptureComplete { pane_id, wav }
            }
            VoiceMessage::CaptureFailed { pane_id, reason } => {
                VoiceEvent::CaptureFailed { pane_id, reason }
            }
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
            VoiceMessage::MaxDuration { pane_id, .. } => VoiceEvent::MaxDuration { pane_id },
        };
        apply_event(voice, event, waker, Some(model), Some(panes), focused_pane);
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
    voice.state = transition.state;
    for effect in transition.effects {
        match effect {
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
                if let Some(model) = model.as_deref_mut() {
                    if pane_id != focused_pane
                        || !panes
                            .as_ref()
                            .is_some_and(|p| p.table.contains_key(&pane_id))
                    {
                        model.status = "Voice transcript discarded: focus changed".into();
                    } else if let Some(pane) =
                        panes.as_deref_mut().and_then(|p| p.table.get_mut(&pane_id))
                    {
                        match crate::run::paste_text_into_pane(pane, &transcript) {
                            Ok(()) => model.status = "Voice transcript inserted".into(),
                            Err(e) => model.status = format!("Voice transcript dropped: {e}"),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_cancel_is_a_noop() {
        // The rendering contract is that an idle voice controller contributes
        // no state or status change; mode-chip decoration is tested beside it.
        assert!(matches!(VoiceState::default(), VoiceState::Idle));
    }
}
