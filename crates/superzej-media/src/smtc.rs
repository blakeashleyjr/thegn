//! Windows backend over the System Media Transport Controls (SMTC) session
//! manager (`windows::Media::Control`). Implements [`MediaBackend`] against
//! whatever app currently owns the system media session (Spotify, browsers,
//! Groove, …).
//!
//! Push mode (`caps.signals = true`): a [`SmtcWatch`] subscribes to the session
//! manager's `CurrentSessionChanged` and the current session's
//! `PlaybackInfoChanged`/`MediaPropertiesChanged`, and rebinds to the new session
//! whenever the active app changes — so the badge updates event-driven (the
//! ~0%-idle contract) instead of polling.
//!
//! All WinRT calls run inside `spawn_blocking` and return plain owned data
//! (`String`/`bool`/`MediaState`), so no COM object crosses an `.await`. The
//! pure enum/tick decoding lives in [`crate::smtc_decode`] (Linux-testable).

use std::sync::{Arc, Mutex};

use windows::Foundation::{EventRegistrationToken, TypedEventHandler};
use windows::Media::Control::GlobalSystemMediaTransportControlsSessionManager as Manager;
use windows::Media::Control::{
    GlobalSystemMediaTransportControlsSession as Session,
    GlobalSystemMediaTransportControlsSessionManager,
};
use windows::Media::MediaPlaybackAutoRepeatMode;

use crate::MediaWatch;

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

    /// Subscribe to SMTC session/playback events for push updates. Rebinds to the
    /// new current session on `CurrentSessionChanged`, so it never freezes when
    /// the active app changes.
    pub async fn watch(&self) -> Result<SmtcWatch, MediaError> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<()>();
        let sub = run_blocking(move || subscribe(tx)).await?;
        Ok(SmtcWatch { _sub: sub, rx })
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
            signals: true, // push via CurrentSessionChanged/PlaybackInfoChanged
            seek: true,
            art: false,
            queue: false,
            abs_volume: false,
            chapters: false,
            fullscreen: false,
        }
    }
}

// === push watcher ==========================================================

/// The per-session event bindings, swapped out whenever the current session
/// changes. Tokens are removed on drop / rebind.
#[derive(Default)]
struct SessionBind {
    session: Option<Session>,
    info_token: Option<EventRegistrationToken>,
    props_token: Option<EventRegistrationToken>,
}

impl SessionBind {
    /// Detach the current session's handlers (best-effort).
    fn clear(&mut self) {
        if let Some(sess) = self.session.take() {
            if let Some(t) = self.info_token.take() {
                let _ = sess.RemovePlaybackInfoChanged(t);
            }
            if let Some(t) = self.props_token.take() {
                let _ = sess.RemoveMediaPropertiesChanged(t);
            }
        }
    }
}

/// Live SMTC subscription — kept alive by [`SmtcWatch`]; drop detaches handlers.
struct Subscription {
    manager: Manager,
    session_changed_token: Option<EventRegistrationToken>,
    bind: Arc<Mutex<SessionBind>>,
}

impl Drop for Subscription {
    fn drop(&mut self) {
        if let Some(t) = self.session_changed_token.take() {
            let _ = self.manager.RemoveCurrentSessionChanged(t);
        }
        self.bind.lock().unwrap().clear();
    }
}

/// (Re)bind `PlaybackInfoChanged` + `MediaPropertiesChanged` on the manager's
/// current session, replacing any prior bindings. Each fires `tx`.
fn rebind(
    manager: &Manager,
    bind: &Arc<Mutex<SessionBind>>,
    tx: &tokio::sync::mpsc::UnboundedSender<()>,
) {
    let mut b = bind.lock().unwrap();
    b.clear();
    let Ok(session) = manager.GetCurrentSession() else {
        return;
    };
    let tx_info = tx.clone();
    b.info_token = session
        .PlaybackInfoChanged(&TypedEventHandler::new(move |_, _| {
            let _ = tx_info.send(());
            Ok(())
        }))
        .ok();
    let tx_props = tx.clone();
    b.props_token = session
        .MediaPropertiesChanged(&TypedEventHandler::new(move |_, _| {
            let _ = tx_props.send(());
            Ok(())
        }))
        .ok();
    b.session = Some(session);
}

/// Build the full subscription: bind the current session now, then subscribe to
/// `CurrentSessionChanged` to rebind + tick on app swaps.
fn subscribe(tx: tokio::sync::mpsc::UnboundedSender<()>) -> Result<Subscription, MediaError> {
    let manager = manager().map_err(win_err)?;
    let bind = Arc::new(Mutex::new(SessionBind::default()));
    rebind(&manager, &bind, &tx);

    let mgr_for_handler = manager.clone();
    let bind_for_handler = bind.clone();
    let session_changed_token = manager
        .CurrentSessionChanged(&TypedEventHandler::new(move |_, _| {
            rebind(&mgr_for_handler, &bind_for_handler, &tx);
            Ok(())
        }))
        .map_err(win_err)?;

    Ok(Subscription {
        manager,
        session_changed_token: Some(session_changed_token),
        bind,
    })
}

/// A push watcher over SMTC session events. Each `changed()` resolves when the
/// playback info, media properties, or current session change.
pub struct SmtcWatch {
    _sub: Subscription,
    rx: tokio::sync::mpsc::UnboundedReceiver<()>,
}

impl MediaWatch for SmtcWatch {
    fn changed(
        &mut self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + '_>> {
        Box::pin(async move { self.rx.recv().await.is_some() })
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
