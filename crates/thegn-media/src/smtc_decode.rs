//! Pure decoders for Windows SMTC values — no `windows` dependency, so they
//! compile and unit-test on a Linux CI box. [`smtc`](crate::smtc) (Windows only)
//! passes the raw WinRT enum discriminants + `TimeSpan` tick counts in here.

use std::time::Duration;

use crate::model::{LoopMode, PlaybackState};
use crate::{MediaCaps, MediaError};

/// Raw, best-effort reads from one SMTC session's playback controls.
/// `None` means the individual WinRT property read failed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ControlFlags {
    pub(crate) next: Option<bool>,
    pub(crate) previous: Option<bool>,
    pub(crate) position: Option<bool>,
    pub(crate) shuffle: Option<bool>,
    pub(crate) repeat: Option<bool>,
}

/// Action availability derived from a session's live playback controls.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct SessionCaps {
    pub(crate) can_go_next: bool,
    pub(crate) can_go_previous: bool,
    pub(crate) can_seek: bool,
    pub(crate) shuffle: bool,
    pub(crate) loop_mode: bool,
}

/// Missing Controls or any unreadable individual flag is conservatively false.
pub(crate) fn session_caps(flags: Option<ControlFlags>) -> SessionCaps {
    let flags = flags.unwrap_or_default();
    SessionCaps {
        can_go_next: flags.next.unwrap_or(false),
        can_go_previous: flags.previous.unwrap_or(false),
        can_seek: flags.position.unwrap_or(false),
        shuffle: flags.shuffle.unwrap_or(false),
        loop_mode: flags.repeat.unwrap_or(false),
    }
}

/// Overlay capabilities that vary with the live SMTC session.
pub(crate) fn snapshot_caps(base: MediaCaps, session: SessionCaps) -> MediaCaps {
    MediaCaps {
        shuffle: session.shuffle,
        loop_mode: session.loop_mode,
        seek: session.can_seek,
        ..base
    }
}

/// Decode timeline ticks, dropping a corrupt position beyond a known end.
pub(crate) fn timeline(
    position_ticks: Option<i64>,
    end_ticks: Option<i64>,
) -> (Option<Duration>, Option<Duration>) {
    let length = end_ticks.and_then(duration_from_ticks);
    let position = position_ticks
        .and_then(duration_from_ticks)
        .filter(|position| length.is_none_or(|length| *position <= length));
    (position, length)
}

/// Distinguish failure to open the SMTC manager from an empty player listing.
pub(crate) fn manager_unavailable(detail: &str) -> MediaError {
    MediaError::Unavailable(format!("SMTC session manager: {detail}"))
}

/// Convert an explicit SMTC refusal into a backend error rather than success.
pub(crate) fn accepted(operation: &str, accepted: bool) -> Result<(), MediaError> {
    if accepted {
        Ok(())
    } else {
        Err(MediaError::Backend(format!(
            "{operation} rejected by the player"
        )))
    }
}

/// Calculate a relative seek target in WinRT ticks. Negative current positions
/// are malformed; forward overflow is rejected, while backward seeks clamp at
/// the beginning of the track.
pub(crate) fn seek_target(current: i64, delta: i64, forward: bool) -> Option<i64> {
    if current < 0 || delta < 0 {
        return None;
    }
    if forward {
        current.checked_add(delta)
    } else {
        Some(current.saturating_sub(delta).max(0))
    }
}

/// Map a `GlobalSystemMediaTransportControlsSessionPlaybackStatus` discriminant
/// onto the normalized axis. WinRT values: Closed=0, Opened=1, Changing=2,
/// Stopped=3, Playing=4, Paused=5.
pub(crate) fn playback_state_from_status(raw: i32) -> PlaybackState {
    match raw {
        4 => PlaybackState::Playing,
        5 => PlaybackState::Paused,
        _ => PlaybackState::Stopped,
    }
}

/// Map a `MediaPlaybackAutoRepeatMode` discriminant onto [`LoopMode`].
/// WinRT values: None=0, Track=1, List=2.
pub(crate) fn loop_from_repeat(raw: i32) -> LoopMode {
    match raw {
        1 => LoopMode::Track,
        2 => LoopMode::Playlist,
        _ => LoopMode::None,
    }
}

/// The `MediaPlaybackAutoRepeatMode` discriminant for a [`LoopMode`].
pub(crate) fn loop_to_repeat(mode: LoopMode) -> i32 {
    match mode {
        LoopMode::None => 0,
        LoopMode::Track => 1,
        LoopMode::Playlist => 2,
    }
}

/// A WinRT `TimeSpan` is a count of 100-nanosecond ticks. `None` for a
/// non-positive span (no position / unknown length).
pub(crate) fn duration_from_ticks(ticks: i64) -> Option<Duration> {
    if ticks > 0 {
        Some(Duration::from_nanos((ticks as u64).saturating_mul(100)))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_refusal_is_an_error() {
        assert_eq!(accepted("next", true), Ok(()));
        assert_eq!(
            accepted("next", false),
            Err(MediaError::Backend("next rejected by the player".into()))
        );
    }

    #[test]
    fn relative_seek_checks_overflow_and_clamps_underflow() {
        assert_eq!(seek_target(i64::MAX - 1, 10, true), None);
        assert_eq!(seek_target(5, 10, false), Some(0));
        assert_eq!(seek_target(5, 2, false), Some(3));
        assert_eq!(seek_target(5, 2, true), Some(7));
        assert_eq!(seek_target(-1, 1, true), None);
    }

    #[test]
    fn status_maps_active_states() {
        assert_eq!(playback_state_from_status(4), PlaybackState::Playing);
        assert_eq!(playback_state_from_status(5), PlaybackState::Paused);
        assert_eq!(playback_state_from_status(3), PlaybackState::Stopped);
        assert_eq!(playback_state_from_status(0), PlaybackState::Stopped);
        assert_eq!(playback_state_from_status(99), PlaybackState::Stopped);
    }

    #[test]
    fn repeat_roundtrips() {
        for mode in [LoopMode::None, LoopMode::Track, LoopMode::Playlist] {
            assert_eq!(loop_from_repeat(loop_to_repeat(mode)), mode);
        }
        assert_eq!(loop_from_repeat(0), LoopMode::None);
        assert_eq!(loop_from_repeat(1), LoopMode::Track);
        assert_eq!(loop_from_repeat(2), LoopMode::Playlist);
        assert_eq!(loop_from_repeat(7), LoopMode::None);
    }

    #[test]
    fn ticks_to_duration() {
        // 248 s = 248 * 10_000_000 ticks.
        assert_eq!(
            duration_from_ticks(2_480_000_000),
            Some(Duration::from_secs(248))
        );
        assert_eq!(duration_from_ticks(0), None);
        assert_eq!(duration_from_ticks(-5), None);
    }

    #[test]
    fn missing_controls_disable_every_session_action() {
        assert_eq!(session_caps(None), SessionCaps::default());
        assert_eq!(
            session_caps(Some(ControlFlags::default())),
            SessionCaps::default()
        );
    }

    #[test]
    fn session_caps_follow_each_flag() {
        let disabled = ControlFlags {
            next: Some(false),
            previous: Some(false),
            position: Some(false),
            shuffle: Some(false),
            repeat: Some(false),
        };
        let cases = [
            (
                ControlFlags {
                    next: Some(true),
                    ..disabled
                },
                SessionCaps {
                    can_go_next: true,
                    ..SessionCaps::default()
                },
            ),
            (
                ControlFlags {
                    previous: Some(true),
                    ..disabled
                },
                SessionCaps {
                    can_go_previous: true,
                    ..SessionCaps::default()
                },
            ),
            (
                ControlFlags {
                    position: Some(true),
                    ..disabled
                },
                SessionCaps {
                    can_seek: true,
                    ..SessionCaps::default()
                },
            ),
            (
                ControlFlags {
                    shuffle: Some(true),
                    ..disabled
                },
                SessionCaps {
                    shuffle: true,
                    ..SessionCaps::default()
                },
            ),
            (
                ControlFlags {
                    repeat: Some(true),
                    ..disabled
                },
                SessionCaps {
                    loop_mode: true,
                    ..SessionCaps::default()
                },
            ),
        ];

        for (flags, expected) in cases {
            assert_eq!(session_caps(Some(flags)), expected);
        }
    }

    #[test]
    fn snapshot_caps_override_only_session_axes() {
        let base = MediaCaps {
            shuffle: true,
            loop_mode: true,
            volume: true,
            playlists: true,
            signals: true,
            seek: true,
            art: true,
            queue: true,
            abs_volume: true,
            chapters: true,
            fullscreen: true,
        };

        assert_eq!(
            snapshot_caps(base, SessionCaps::default()),
            MediaCaps {
                shuffle: false,
                loop_mode: false,
                volume: true,
                playlists: true,
                signals: true,
                seek: false,
                art: true,
                queue: true,
                abs_volume: true,
                chapters: true,
                fullscreen: true,
            }
        );
    }

    #[test]
    fn timeline_drops_position_past_end() {
        assert_eq!(
            timeline(Some(20), Some(10)),
            (None, Some(Duration::from_nanos(1_000)))
        );
        assert_eq!(
            timeline(Some(5), Some(10)),
            (
                Some(Duration::from_nanos(500)),
                Some(Duration::from_nanos(1_000))
            )
        );
        assert_eq!(
            timeline(Some(5), None),
            (Some(Duration::from_nanos(500)), None)
        );
        assert_eq!(
            timeline(Some(-1), Some(10)),
            (None, Some(Duration::from_nanos(1_000)))
        );
        assert_eq!(timeline(None, None), (None, None));
    }

    #[test]
    fn manager_failure_is_unavailable_not_no_player() {
        assert!(matches!(
            manager_unavailable("x"),
            MediaError::Unavailable(message) if message.contains("x")
        ));
        assert!(!matches!(manager_unavailable("x"), MediaError::NoPlayer));
    }
}
