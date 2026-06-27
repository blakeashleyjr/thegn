//! Pure decoders for Windows SMTC values — no `windows` dependency, so they
//! compile and unit-test on a Linux CI box. [`smtc`](crate::smtc) (Windows only)
//! passes the raw WinRT enum discriminants + `TimeSpan` tick counts in here.

use std::time::Duration;

use crate::model::{LoopMode, PlaybackState};

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
}
