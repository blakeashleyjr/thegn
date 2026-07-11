//! Windows backend over the System Media Transport Controls (SMTC) session
//! manager (`windows::Media::Control`). Implements [`MediaBackend`] against
//! whatever app currently owns the system media session (Spotify, browsers,
//! Groove, …).
//!
//! Poll mode for now (`caps.signals = false`): the host re-snapshots on the
//! `[media] poll_interval_secs` ticker. A push watcher over
//! `PlaybackInfoChanged`/`MediaPropertiesChanged` is a future enhancement.
//!
//! All WinRT calls run inside `spawn_blocking` and return plain owned data
//! (`String`/`bool`/`MediaState`), so no COM object crosses an `.await`. The
//! pure enum/tick decoding lives in [`crate::smtc_decode`] (Linux-testable).

use windows::Media::Control::GlobalSystemMediaTransportControlsSessionManager as Manager;
use windows::Media::Control::{
    GlobalSystemMediaTransportControlsSession as Session,
    GlobalSystemMediaTransportControlsSessionManager,
};
use windows::Media::MediaPlaybackAutoRepeatMode;

use crate::model::{LoopMode, MediaKind, MediaState};
use crate::smtc_decode::{
    duration_from_ticks, loop_from_repeat, loop_to_repeat, playback_state_from_status,
};
use crate::{MediaBackend, MediaCaps, MediaError};

/// Stateless SMTC controller — the session manager is opened fresh per call
/// (cheap, and avoids holding a COM object across `.await`).
pub struct Smtc;

impl Smtc {
    /// Probe that the session manager is obtainable; `None` ⇒ media inert.
    pub async fn connect() -> Option<Self> {
        let ok = tokio::task::spawn_blocking(|| manager().is_ok())
            .await
            .unwrap_or(false);
        ok.then_some(Smtc)
    }

    /// The current session's source app id (SMTC's notion of "player").
    pub async fn list_players(&self) -> Vec<String> {
        run_blocking(|| {
            let session = current_session()?;
            let id = session
                .SourceAppUserModelId()
                .map(|h| h.to_string())
                .unwrap_or_default();
            Ok(if id.is_empty() { Vec::new() } else { vec![id] })
        })
        .await
        .unwrap_or_default()
    }
}

impl MediaBackend for Smtc {
    async fn snapshot(&self) -> Result<Option<MediaState>, MediaError> {
        run_blocking(snapshot_blocking).await
    }

    async fn play_pause(&self) -> Result<(), MediaError> {
        run_blocking(|| {
            current_session()?
                .TryTogglePlayPauseAsync()
                .and_then(|op| op.get())
                .map_err(win_err)?;
            Ok(())
        })
        .await
    }
    async fn next(&self) -> Result<(), MediaError> {
        run_blocking(|| {
            current_session()?
                .TrySkipNextAsync()
                .and_then(|op| op.get())
                .map_err(win_err)?;
            Ok(())
        })
        .await
    }
    async fn previous(&self) -> Result<(), MediaError> {
        run_blocking(|| {
            current_session()?
                .TrySkipPreviousAsync()
                .and_then(|op| op.get())
                .map_err(win_err)?;
            Ok(())
        })
        .await
    }
    async fn set_shuffle(&self, on: bool) -> Result<(), MediaError> {
        run_blocking(move || {
            current_session()?
                .TryChangeShuffleActiveAsync(on)
                .and_then(|op| op.get())
                .map_err(win_err)?;
            Ok(())
        })
        .await
    }
    async fn set_loop(&self, mode: LoopMode) -> Result<(), MediaError> {
        run_blocking(move || {
            current_session()?
                .TryChangeAutoRepeatModeAsync(MediaPlaybackAutoRepeatMode(loop_to_repeat(mode)))
                .and_then(|op| op.get())
                .map_err(win_err)?;
            Ok(())
        })
        .await
    }
    async fn volume_step(&self, _delta: f64) -> Result<(), MediaError> {
        Ok(()) // SMTC exposes no volume control; caps().volume == false
    }

    async fn playlists(&self) -> Result<Vec<crate::model::Playlist>, MediaError> {
        Ok(Vec::new()) // SMTC has no playlist enumeration
    }
    async fn activate_playlist(&self, _id: &str) -> Result<(), MediaError> {
        Ok(())
    }

    async fn seek(&self, offset: std::time::Duration, forward: bool) -> Result<(), MediaError> {
        run_blocking(move || {
            let session = current_session()?;
            let cur = session
                .GetTimelineProperties()
                .and_then(|t| t.Position())
                .map(|ts| ts.Duration)
                .unwrap_or(0);
            let delta = ticks_from_duration(offset);
            let target = if forward {
                cur + delta
            } else {
                (cur - delta).max(0)
            };
            session
                .TryChangePlaybackPositionAsync(target)
                .and_then(|op| op.get())
                .map_err(win_err)?;
            Ok(())
        })
        .await
    }
    async fn set_position(
        &self,
        pos: std::time::Duration,
        _track_id: Option<&str>,
    ) -> Result<(), MediaError> {
        run_blocking(move || {
            current_session()?
                .TryChangePlaybackPositionAsync(ticks_from_duration(pos))
                .and_then(|op| op.get())
                .map_err(win_err)?;
            Ok(())
        })
        .await
    }

    fn caps(&self) -> MediaCaps {
        MediaCaps {
            shuffle: true,
            loop_mode: true,
            volume: false,
            playlists: false,
            signals: false, // host polls on [media] poll_interval_secs
            seek: true,
            art: false,
            queue: false,
            abs_volume: false,
            chapters: false,
            fullscreen: false,
        }
    }
}

/// A [`std::time::Duration`] as WinRT 100-nanosecond ticks (inverse of
/// [`duration_from_ticks`]).
fn ticks_from_duration(d: std::time::Duration) -> i64 {
    (d.as_nanos() / 100).min(i64::MAX as u128) as i64
}

// === blocking COM helpers ==================================================

fn manager() -> windows::core::Result<GlobalSystemMediaTransportControlsSessionManager> {
    Manager::RequestAsync()?.get()
}

fn current_session() -> Result<Session, MediaError> {
    manager()
        .and_then(|m| m.GetCurrentSession())
        .map_err(|_| MediaError::NoPlayer)
}

fn win_err(e: windows::core::Error) -> MediaError {
    MediaError::Backend(e.to_string())
}

/// Fold the current SMTC session into a [`MediaState`]. `Ok(None)` when no app
/// owns the media session.
fn snapshot_blocking() -> Result<Option<MediaState>, MediaError> {
    let mgr = manager().map_err(win_err)?;
    let session = match mgr.GetCurrentSession() {
        Ok(s) => s,
        Err(_) => return Ok(None),
    };

    let info = session.GetPlaybackInfo().map_err(win_err)?;
    let state = playback_state_from_status(info.PlaybackStatus().map_err(win_err)?.0);
    let shuffle = info.IsShuffleActive().ok().and_then(|r| r.Value().ok());
    let loop_mode = info
        .AutoRepeatMode()
        .ok()
        .and_then(|r| r.Value().ok())
        .map(|m| loop_from_repeat(m.0));
    let controls = info.Controls().ok();
    let can_go_next = controls
        .as_ref()
        .and_then(|c| c.IsNextEnabled().ok())
        .unwrap_or(true);
    let can_go_previous = controls
        .as_ref()
        .and_then(|c| c.IsPreviousEnabled().ok())
        .unwrap_or(true);

    let props = session
        .TryGetMediaPropertiesAsync()
        .and_then(|op| op.get())
        .map_err(win_err)?;
    let title = props.Title().map(|h| h.to_string()).unwrap_or_default();
    let artist = props.Artist().map(|h| h.to_string()).unwrap_or_default();
    let album = props
        .AlbumTitle()
        .map(|h| h.to_string())
        .unwrap_or_default();

    let timeline = session.GetTimelineProperties().ok();
    let position = timeline
        .as_ref()
        .and_then(|t| t.Position().ok())
        .and_then(|ts| duration_from_ticks(ts.Duration));
    let length = timeline
        .as_ref()
        .and_then(|t| t.EndTime().ok())
        .and_then(|ts| duration_from_ticks(ts.Duration));

    let player = session
        .SourceAppUserModelId()
        .map(|h| h.to_string())
        .unwrap_or_default();

    // SMTC exposes a PlaybackType (Music/Video/Image) — map it, falling back to
    // the player-name heuristic.
    let kind = match props.PlaybackType().ok().and_then(|r| r.Value().ok()) {
        Some(t) if t.0 == 3 => MediaKind::Video, // MediaPlaybackType::Video
        Some(t) if t.0 == 1 => MediaKind::Audio, // MediaPlaybackType::Music
        _ => MediaKind::from_hints(&player, None, None),
    };

    Ok(Some(MediaState {
        player,
        title,
        artist,
        album,
        state,
        position,
        length,
        shuffle,
        loop_mode,
        volume: None, // SMTC has no volume
        can_go_next,
        can_go_previous,
        art_url: None, // thumbnail is a stream, not fetched in v1
        kind,
        can_seek: true, // SMTC position change is generally available
        track_id: None,
    }))
}

/// Run blocking COM work off the async runtime, flattening the join error.
async fn run_blocking<T, F>(f: F) -> Result<T, MediaError>
where
    F: FnOnce() -> Result<T, MediaError> + Send + 'static,
    T: Send + 'static,
{
    match tokio::task::spawn_blocking(f).await {
        Ok(r) => r,
        Err(e) => Err(MediaError::Backend(format!("smtc task: {e}"))),
    }
}
