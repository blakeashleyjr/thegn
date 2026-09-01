//! Pure voice provider contract and toggle-to-talk state machine.
//!
//! This module intentionally has no process, terminal, audio, filesystem, or
//! clock dependencies.  The host supplies integer timestamps and executes the
//! effects emitted by [`reduce`].

use crate::seam::{ErrorClass, Probe, ProbeReport, SeamError};
use serde::{Deserialize, Serialize};

/// Optional operations exposed by a voice provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct VoiceCaps {
    pub transcribe: bool,
}

impl Default for VoiceCaps {
    fn default() -> Self {
        Self { transcribe: true }
    }
}

/// Classified failures from a voice provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VoiceError {
    NotConfigured,
    NotInstalled(String),
    InvalidInput(String),
    Failed(String),
    Unsupported(&'static str),
}

impl std::fmt::Display for VoiceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotConfigured => f.write_str("voice is not configured"),
            Self::NotInstalled(program) => write!(f, "voice command is not installed: {program}"),
            Self::InvalidInput(reason) => write!(f, "invalid voice input: {reason}"),
            Self::Failed(reason) => write!(f, "voice transcription failed: {reason}"),
            Self::Unsupported(op) => write!(f, "{op} is not supported by this voice provider"),
        }
    }
}

impl std::error::Error for VoiceError {}

impl SeamError for VoiceError {
    fn class(&self) -> ErrorClass {
        match self {
            Self::NotConfigured => ErrorClass::NotConfigured,
            Self::NotInstalled(_) => ErrorClass::NotInstalled,
            Self::Unsupported(_) => ErrorClass::Unsupported,
            Self::InvalidInput(_) | Self::Failed(_) => ErrorClass::Other,
        }
    }

    fn unsupported(op: &'static str) -> Self {
        Self::Unsupported(op)
    }
}

/// A synchronous voice provider.  Implementations also provide the generic
/// [`Probe`] description used by doctor; callers run this blocking operation
/// away from the event loop.
pub trait VoiceProvider: Probe + Send + Sync {
    fn caps(&self) -> VoiceCaps;
    fn transcribe_push_to_talk(&self, wav: &[u8]) -> Result<String, VoiceError>;
}

/// The pane that owns the current utterance and the state of voice capture.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub enum VoiceState {
    #[default]
    Idle,
    Recording {
        pane_id: u32,
        started_at: u64,
    },
    Transcribing {
        pane_id: u32,
        request_id: u64,
    },
}

impl VoiceState {
    pub fn is_recording(&self) -> bool {
        matches!(self, Self::Recording { .. })
    }
}

/// Input to the pure voice reducer.  `now_ms` is supplied by the host so this
/// module remains deterministic and testable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VoiceEvent {
    Start {
        pane_id: u32,
        now_ms: u64,
    },
    Stop {
        pane_id: u32,
    },
    CaptureComplete {
        pane_id: u32,
        wav: Vec<u8>,
    },
    CaptureFailed {
        pane_id: u32,
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
    Cancel,
    MaxDuration {
        pane_id: u32,
    },
}

/// Side effects for the host to execute after a state transition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VoiceEffect {
    StartCapture {
        pane_id: u32,
    },
    StopCapture {
        pane_id: u32,
    },
    Transcribe {
        pane_id: u32,
        request_id: u64,
        wav: Vec<u8>,
    },
    Inject {
        pane_id: u32,
        transcript: String,
    },
    Notify(String),
}

/// Result of one pure event reduction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VoiceTransition {
    pub state: VoiceState,
    pub effects: Vec<VoiceEffect>,
}

impl VoiceTransition {
    fn new(state: VoiceState, effects: Vec<VoiceEffect>) -> Self {
        Self { state, effects }
    }
    fn unchanged(state: &VoiceState) -> Self {
        Self::new(state.clone(), Vec::new())
    }
}

/// Reduce one voice event.  Legal transitions are `Idle → Recording →
/// Transcribing → Idle`; stale, duplicate, and mismatched-pane events are
/// ignored.  A request id is allocated only when capture completes, which
/// makes cancellation invalidate every result that was already in flight.
pub fn reduce(state: &VoiceState, next_request_id: &mut u64, event: VoiceEvent) -> VoiceTransition {
    match (state, event) {
        (VoiceState::Idle, VoiceEvent::Start { pane_id, now_ms }) => VoiceTransition::new(
            VoiceState::Recording {
                pane_id,
                started_at: now_ms,
            },
            vec![VoiceEffect::StartCapture { pane_id }],
        ),
        (
            VoiceState::Recording { pane_id, .. },
            VoiceEvent::Stop {
                pane_id: event_pane,
            },
        ) if *pane_id == event_pane => VoiceTransition::new(
            state.clone(),
            vec![VoiceEffect::StopCapture { pane_id: *pane_id }],
        ),
        (
            VoiceState::Recording { pane_id, .. },
            VoiceEvent::MaxDuration {
                pane_id: event_pane,
            },
        ) if *pane_id == event_pane => VoiceTransition::new(
            state.clone(),
            vec![
                VoiceEffect::StopCapture { pane_id: *pane_id },
                VoiceEffect::Notify("voice capture limit reached".into()),
            ],
        ),
        (
            VoiceState::Recording { pane_id, .. },
            VoiceEvent::CaptureComplete {
                pane_id: event_pane,
                wav,
            },
        ) if *pane_id == event_pane => {
            *next_request_id = next_request_id.saturating_add(1);
            let request_id = *next_request_id;
            VoiceTransition::new(
                VoiceState::Transcribing {
                    pane_id: *pane_id,
                    request_id,
                },
                vec![VoiceEffect::Transcribe {
                    pane_id: *pane_id,
                    request_id,
                    wav,
                }],
            )
        }
        (
            VoiceState::Recording { pane_id, .. },
            VoiceEvent::CaptureFailed {
                pane_id: event_pane,
                reason,
            },
        ) if *pane_id == event_pane => VoiceTransition::new(
            VoiceState::Idle,
            vec![VoiceEffect::Notify(format!(
                "voice capture failed: {reason}"
            ))],
        ),
        (
            VoiceState::Transcribing {
                pane_id,
                request_id,
            },
            VoiceEvent::TranscriptSucceeded {
                pane_id: event_pane,
                request_id: event_request,
                transcript,
            },
        ) if *pane_id == event_pane && *request_id == event_request => {
            let text = transcript.trim().to_string();
            let effects = if text.is_empty() {
                Vec::new()
            } else {
                vec![VoiceEffect::Inject {
                    pane_id: *pane_id,
                    transcript: text,
                }]
            };
            VoiceTransition::new(VoiceState::Idle, effects)
        }
        (
            VoiceState::Transcribing {
                pane_id,
                request_id,
            },
            VoiceEvent::TranscriptFailed {
                pane_id: event_pane,
                request_id: event_request,
                reason,
            },
        ) if *pane_id == event_pane && *request_id == event_request => VoiceTransition::new(
            VoiceState::Idle,
            vec![VoiceEffect::Notify(format!(
                "voice transcription failed: {reason}"
            ))],
        ),
        (VoiceState::Recording { pane_id, .. }, VoiceEvent::Cancel) => VoiceTransition::new(
            VoiceState::Idle,
            vec![
                VoiceEffect::StopCapture { pane_id: *pane_id },
                VoiceEffect::Notify("voice capture cancelled".into()),
            ],
        ),
        (VoiceState::Transcribing { .. }, VoiceEvent::Cancel) => VoiceTransition::new(
            VoiceState::Idle,
            vec![VoiceEffect::Notify("voice cancelled".into())],
        ),
        (current, _) => VoiceTransition::unchanged(current),
    }
}

/// A small helper for provider implementations that need a generic probe
/// report without inventing another doctor surface.
pub fn unavailable_probe(reason: impl Into<String>) -> ProbeReport {
    ProbeReport::new(
        "voice",
        "command",
        crate::seam::Availability::Unavailable(reason.into()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reduce_from(state: &VoiceState, id: &mut u64, event: VoiceEvent) -> VoiceTransition {
        reduce(state, id, event)
    }

    #[test]
    fn provider_error_classes_are_useful_to_callers() {
        assert_eq!(VoiceError::NotConfigured.class(), ErrorClass::NotConfigured);
        assert_eq!(
            VoiceError::NotInstalled("mic".into()).class(),
            ErrorClass::NotInstalled
        );
        assert_eq!(
            VoiceError::InvalidInput("wav".into()).class(),
            ErrorClass::Other
        );
        assert!(VoiceError::unsupported("transcribe").falls_through());
    }

    #[test]
    fn every_happy_path_transition_is_explicit() {
        let mut id = 0;
        let idle = VoiceState::Idle;
        let started = reduce_from(
            &idle,
            &mut id,
            VoiceEvent::Start {
                pane_id: 7,
                now_ms: 11,
            },
        );
        assert_eq!(
            started.state,
            VoiceState::Recording {
                pane_id: 7,
                started_at: 11
            }
        );
        assert_eq!(
            started.effects,
            vec![VoiceEffect::StartCapture { pane_id: 7 }]
        );
        let stopped = reduce_from(&started.state, &mut id, VoiceEvent::Stop { pane_id: 7 });
        assert_eq!(stopped.state, started.state);
        assert_eq!(
            stopped.effects,
            vec![VoiceEffect::StopCapture { pane_id: 7 }]
        );
        let transcribing = reduce_from(
            &stopped.state,
            &mut id,
            VoiceEvent::CaptureComplete {
                pane_id: 7,
                wav: vec![1, 2],
            },
        );
        assert_eq!(
            transcribing.state,
            VoiceState::Transcribing {
                pane_id: 7,
                request_id: 1
            }
        );
        assert!(matches!(
            transcribing.effects[0],
            VoiceEffect::Transcribe { request_id: 1, .. }
        ));
        let done = reduce_from(
            &transcribing.state,
            &mut id,
            VoiceEvent::TranscriptSucceeded {
                pane_id: 7,
                request_id: 1,
                transcript: "  hello  ".into(),
            },
        );
        assert_eq!(done.state, VoiceState::Idle);
        assert_eq!(
            done.effects,
            vec![VoiceEffect::Inject {
                pane_id: 7,
                transcript: "hello".into()
            }]
        );
    }

    #[test]
    fn duplicate_and_mismatched_events_do_not_advance_state() {
        let mut id = 0;
        let recording = reduce_from(
            &VoiceState::Idle,
            &mut id,
            VoiceEvent::Start {
                pane_id: 3,
                now_ms: 1,
            },
        )
        .state;
        assert!(
            reduce_from(
                &recording,
                &mut id,
                VoiceEvent::Start {
                    pane_id: 8,
                    now_ms: 2
                }
            )
            .effects
            .is_empty()
        );
        assert!(
            reduce_from(&recording, &mut id, VoiceEvent::Stop { pane_id: 8 })
                .effects
                .is_empty()
        );
        assert_eq!(
            reduce_from(&recording, &mut id, VoiceEvent::Stop { pane_id: 8 }).state,
            recording
        );
    }

    #[test]
    fn cancel_max_failure_and_stale_results_are_safe() {
        let mut id = 0;
        let recording = reduce_from(
            &VoiceState::Idle,
            &mut id,
            VoiceEvent::Start {
                pane_id: 4,
                now_ms: 1,
            },
        )
        .state;
        let cancelled = reduce_from(&recording, &mut id, VoiceEvent::Cancel);
        assert_eq!(cancelled.state, VoiceState::Idle);
        assert!(
            cancelled
                .effects
                .iter()
                .any(|e| matches!(e, VoiceEffect::StopCapture { .. }))
        );
        let failed = reduce_from(
            &recording,
            &mut id,
            VoiceEvent::CaptureFailed {
                pane_id: 4,
                reason: "broken".into(),
            },
        );
        assert_eq!(failed.state, VoiceState::Idle);
        let maxed = reduce_from(&recording, &mut id, VoiceEvent::MaxDuration { pane_id: 4 });
        assert_eq!(maxed.state, recording);
        let mut id = 0;
        let pending = reduce_from(
            &recording,
            &mut id,
            VoiceEvent::CaptureComplete {
                pane_id: 4,
                wav: vec![],
            },
        )
        .state;
        assert!(
            reduce_from(
                &pending,
                &mut id,
                VoiceEvent::TranscriptSucceeded {
                    pane_id: 4,
                    request_id: 99,
                    transcript: "bad".into()
                },
            )
            .effects
            .is_empty()
        );
        assert!(
            reduce_from(
                &VoiceState::Idle,
                &mut id,
                VoiceEvent::CaptureComplete {
                    pane_id: 4,
                    wav: vec![1]
                },
            )
            .effects
            .is_empty()
        );
    }

    #[test]
    fn empty_and_whitespace_transcripts_are_not_injected() {
        let mut id = 0;
        let pending = reduce_from(
            &VoiceState::Recording {
                pane_id: 1,
                started_at: 0,
            },
            &mut id,
            VoiceEvent::CaptureComplete {
                pane_id: 1,
                wav: vec![1],
            },
        )
        .state;
        for transcript in [String::new(), " \n\t ".into()] {
            let result = reduce_from(
                &pending,
                &mut id,
                VoiceEvent::TranscriptSucceeded {
                    pane_id: 1,
                    request_id: 1,
                    transcript,
                },
            );
            assert_eq!(result.state, VoiceState::Idle);
            assert!(result.effects.is_empty());
        }
    }
}
