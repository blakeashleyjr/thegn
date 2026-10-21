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
//! Rebinds are generation-fenced outside the bind lock, and a failed rebind ends
//! the watch so the host reconnects instead of trusting stale signal capability.
//!
//! All WinRT calls run inside `spawn_blocking` and return plain owned data
//! (`String`/`bool`/`MediaState`), so no COM object crosses an `.await`. The
//! pure enum/tick decoding lives in [`crate::smtc_decode`] (Linux-testable).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
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
    BindState, ControlFlags, accepted, lock_unpoisoned, loop_from_repeat, loop_to_repeat,
    manager_unavailable, playback_state_from_status, register_pair, seek_target, session_caps,
    snapshot_caps, timeline,
};
use crate::{MediaBackend, MediaCaps, MediaError, MediaSnapshot};

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
        let (tx, rx) = tokio::sync::mpsc::channel::<()>(1);
        let sub = run_blocking(move || subscribe(tx)).await?;
        let failed = sub.failed.clone();
        Ok(SmtcWatch {
            _sub: sub,
            rx,
            failed,
        })
    }

    /// The current session's source app id (SMTC's notion of "player").
    pub async fn list_players(&self) -> Result<Vec<String>, MediaError> {
        run_blocking(|| {
            let mgr = manager().map_err(|error| manager_unavailable(&error.to_string()))?;
            let session = match session_if_present(mgr.GetCurrentSession()) {
                Some(session) => session,
                None => return Ok(Vec::new()),
            };
            let id = session.SourceAppUserModelId().map_err(win_err)?.to_string();
            Ok(if id.is_empty() { Vec::new() } else { vec![id] })
        })
        .await
    }
}

fn session_if_present<T, E>(session: Result<T, E>) -> Option<T> {
    match session {
        Ok(session) => Some(session),
        Err(_) => None,
    }
}

impl MediaBackend for Smtc {
    fn snapshot(&self) -> BoxFuture<'_, Result<Option<MediaState>, MediaError>> {
        Box::pin(async move {
            self.snapshot_with_caps()
                .await
                .map(|snapshot| snapshot.map(|snapshot| snapshot.state))
        })
    }

    fn snapshot_with_caps(&self) -> BoxFuture<'_, Result<Option<MediaSnapshot>, MediaError>> {
        Box::pin(async move { run_blocking(snapshot_with_caps_blocking).await })
    }

    fn play_pause(&self) -> BoxFuture<'_, Result<(), MediaError>> {
        Box::pin(async move {
            run_blocking(|| {
                let was_accepted = current_session()?
                    .TryTogglePlayPauseAsync()
                    .and_then(|op| op.get())
                    .map_err(win_err)?;
                accepted("play/pause", was_accepted)?;
                Ok(())
            })
            .await
        })
    }
    fn next(&self) -> BoxFuture<'_, Result<(), MediaError>> {
        Box::pin(async move {
            run_blocking(|| {
                let was_accepted = current_session()?
                    .TrySkipNextAsync()
                    .and_then(|op| op.get())
                    .map_err(win_err)?;
                accepted("next", was_accepted)?;
                Ok(())
            })
            .await
        })
    }
    fn previous(&self) -> BoxFuture<'_, Result<(), MediaError>> {
        Box::pin(async move {
            run_blocking(|| {
                let was_accepted = current_session()?
                    .TrySkipPreviousAsync()
                    .and_then(|op| op.get())
                    .map_err(win_err)?;
                accepted("previous", was_accepted)?;
                Ok(())
            })
            .await
        })
    }
    fn set_shuffle(&self, on: bool) -> BoxFuture<'_, Result<(), MediaError>> {
        Box::pin(async move {
            run_blocking(move || {
                let was_accepted = current_session()?
                    .TryChangeShuffleActiveAsync(on)
                    .and_then(|op| op.get())
                    .map_err(win_err)?;
                accepted("shuffle", was_accepted)?;
                Ok(())
            })
            .await
        })
    }
    fn set_loop(&self, mode: LoopMode) -> BoxFuture<'_, Result<(), MediaError>> {
        Box::pin(async move {
            run_blocking(move || {
                let was_accepted = current_session()?
                    .TryChangeAutoRepeatModeAsync(MediaPlaybackAutoRepeatMode(loop_to_repeat(mode)))
                    .and_then(|op| op.get())
                    .map_err(win_err)?;
                accepted("repeat", was_accepted)?;
                Ok(())
            })
            .await
        })
    }
    fn volume_step(&self, _delta: f64) -> BoxFuture<'_, Result<(), MediaError>> {
        Box::pin(async move {
            Err(MediaError::Unsupported(
                "SMTC does not expose volume control".into(),
            ))
        })
    }

    fn playlists(&self) -> BoxFuture<'_, Result<Vec<crate::model::Playlist>, MediaError>> {
        Box::pin(async move {
            Ok(Vec::new()) // SMTC has no playlist enumeration
        })
    }
    fn activate_playlist<'a>(&'a self, _id: &'a str) -> BoxFuture<'a, Result<(), MediaError>> {
        Box::pin(async move {
            Err(MediaError::Unsupported(
                "SMTC does not expose playlist activation".into(),
            ))
        })
    }

    fn seek(
        &self,
        offset: std::time::Duration,
        forward: bool,
    ) -> BoxFuture<'_, Result<(), MediaError>> {
        Box::pin(async move {
            run_blocking(move || {
                let session = current_session()?;
                let cur = session
                    .GetTimelineProperties()
                    .and_then(|t| t.Position())
                    .map(|ts| ts.Duration)
                    .map_err(win_err)?;
                let delta = ticks_from_duration(offset);
                let target = seek_target(cur, delta, forward).ok_or_else(|| {
                    MediaError::Backend("relative seek position is invalid or overflows".into())
                })?;
                let was_accepted = session
                    .TryChangePlaybackPositionAsync(target)
                    .and_then(|op| op.get())
                    .map_err(win_err)?;
                accepted("seek", was_accepted)?;
                Ok(())
            })
            .await
        })
    }
    fn set_position<'a>(
        &'a self,
        pos: std::time::Duration,
        _track_id: Option<&'a str>,
    ) -> BoxFuture<'a, Result<(), MediaError>> {
        Box::pin(async move {
            run_blocking(move || {
                let was_accepted = current_session()?
                    .TryChangePlaybackPositionAsync(ticks_from_duration(pos))
                    .and_then(|op| op.get())
                    .map_err(win_err)?;
                accepted("set position", was_accepted)?;
                Ok(())
            })
            .await
        })
    }

    fn players(&self) -> BoxFuture<'_, Result<Vec<String>, MediaError>> {
        Box::pin(async move { self.list_players().await })
    }

    /// The SMTC session-event push watcher.
    fn watch(&self) -> BoxFuture<'_, Result<Option<Box<dyn MediaWatch + Send>>, MediaError>> {
        Box::pin(async move {
            Smtc::watch(self)
                .await
                .map(|w| Some(Box::new(w) as Box<dyn MediaWatch + Send>))
        })
    }

    /// Static pre-snapshot upper bound; `snapshot_with_caps` reports live
    /// session shuffle, repeat and seek support.
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
struct SessionBind {
    session: Session,
    info_token: EventRegistrationToken,
    props_token: EventRegistrationToken,
}

/// Remove both event handlers. Attempt both removals even if the first fails.
fn detach(binding: SessionBind) -> Result<(), String> {
    let mut failures = Vec::new();
    if let Err(error) = binding
        .session
        .RemovePlaybackInfoChanged(binding.info_token)
    {
        failures.push(format!("playback-info handler: {error}"));
    }
    if let Err(error) = binding
        .session
        .RemoveMediaPropertiesChanged(binding.props_token)
    {
        failures.push(format!("media-properties handler: {error}"));
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("; "))
    }
}

/// Live SMTC subscription — kept alive by [`SmtcWatch`]; drop detaches handlers.
struct Subscription {
    manager: Manager,
    session_changed_token: Option<EventRegistrationToken>,
    bind: Arc<Mutex<BindState<SessionBind>>>,
    failed: Arc<AtomicBool>,
}

impl Drop for Subscription {
    fn drop(&mut self) {
        if let Some(t) = self.session_changed_token.take() {
            if let Err(error) = self.manager.RemoveCurrentSessionChanged(t) {
                tracing::warn!(target: "thegn::media", %error, "smtc manager handler removal failed");
            }
        }
        let old = lock_unpoisoned(&self.bind).close();
        if let Some(old) = old
            && let Err(error) = detach(old)
        {
            tracing::warn!(target: "thegn::media", error = %error, "smtc handler removal failed");
        }
    }
}

/// (Re)bind `PlaybackInfoChanged` + `MediaPropertiesChanged` on the manager's
/// current session, replacing any prior bindings. Each fires `tx`.
fn rebind(
    manager: &Manager,
    bind: &Arc<Mutex<BindState<SessionBind>>>,
    tx: &tokio::sync::mpsc::Sender<()>,
) -> Result<(), MediaError> {
    let Some((generation, old)) = lock_unpoisoned(bind).begin() else {
        return Ok(());
    };
    if let Some(old) = old
        && let Err(error) = detach(old)
    {
        tracing::warn!(target: "thegn::media", error = %error, "smtc handler removal failed");
    }
    let Ok(session) = manager.GetCurrentSession() else {
        return Ok(());
    };
    let tx_info = tx.clone();
    let tx_props = tx.clone();
    let (info_token, props_token) = register_pair(
        || {
            session
                .PlaybackInfoChanged(&TypedEventHandler::new(move |_, _| {
                    let _ = tx_info.try_send(()); // best-effort: Full means a wake is already pending; Closed means listener is gone
                    Ok(())
                }))
                .map_err(|error| error.to_string())
        },
        || {
            session
                .MediaPropertiesChanged(&TypedEventHandler::new(move |_, _| {
                    let _ = tx_props.try_send(()); // best-effort: Full means a wake is already pending; Closed means listener is gone
                    Ok(())
                }))
                .map_err(|error| error.to_string())
        },
        |token| {
            session
                .RemovePlaybackInfoChanged(token)
                .map_err(|error| error.to_string())
        },
    )?;
    let new = SessionBind {
        session,
        info_token,
        props_token,
    };
    let installed = lock_unpoisoned(bind).install(generation, new);
    if let Err(stale) = installed
        && let Err(error) = detach(stale)
    {
        tracing::warn!(target: "thegn::media", error = %error, "stale smtc handler removal failed");
    }
    Ok(())
}

/// Build the full subscription: bind the current session now, then subscribe to
/// `CurrentSessionChanged` to rebind + tick on app swaps.
fn subscribe(tx: tokio::sync::mpsc::Sender<()>) -> Result<Subscription, MediaError> {
    let manager = manager().map_err(win_err)?;
    let bind = Arc::new(Mutex::new(BindState::<SessionBind>::default()));
    rebind(&manager, &bind, &tx)?;
    let failed = Arc::new(AtomicBool::new(false));

    let mgr_for_handler = manager.clone();
    let bind_for_handler = bind.clone();
    let failed_for_handler = failed.clone();
    let handler = TypedEventHandler::new(move |_, _| {
        if let Err(error) = rebind(&mgr_for_handler, &bind_for_handler, &tx) {
            tracing::warn!(target: "thegn::media", %error, "smtc session rebind failed; ending watcher");
            failed_for_handler.store(true, Ordering::Release);
        }
        let _ = tx.try_send(()); // best-effort: Full means a wake is already pending; Closed means listener is gone
        Ok(())
    });
    let session_changed_token = match manager.CurrentSessionChanged(&handler) {
        Ok(token) => token,
        Err(error) => {
            let old = lock_unpoisoned(&bind).close();
            if let Some(old) = old
                && let Err(removal_error) = detach(old)
            {
                tracing::warn!(target: "thegn::media", error = %removal_error, "smtc handler removal failed");
            }
            return Err(win_err(error));
        }
    };

    Ok(Subscription {
        manager,
        session_changed_token: Some(session_changed_token),
        bind,
        failed,
    })
}

/// A push watcher over SMTC session events. Each `changed()` resolves when the
/// playback info, media properties, or current session change.
pub struct SmtcWatch {
    _sub: Subscription,
    rx: tokio::sync::mpsc::Receiver<()>,
    failed: Arc<AtomicBool>,
}

impl MediaWatch for SmtcWatch {
    fn changed(
        &mut self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + '_>> {
        Box::pin(async move {
            !self.failed.load(Ordering::Acquire)
                && self.rx.recv().await.is_some()
                && !self.failed.load(Ordering::Acquire)
        })
    }
}

/// A [`std::time::Duration`] as WinRT 100-nanosecond ticks (inverse of
/// [`crate::smtc_decode::duration_from_ticks`]).
fn ticks_from_duration(d: std::time::Duration) -> i64 {
    (d.as_nanos() / 100).min(i64::MAX as u128) as i64
}

// === blocking COM helpers ==================================================

fn manager() -> windows::core::Result<GlobalSystemMediaTransportControlsSessionManager> {
    Manager::RequestAsync()?.get()
}

fn current_session() -> Result<Session, MediaError> {
    let mgr = manager().map_err(|error| manager_unavailable(&error.to_string()))?;
    mgr.GetCurrentSession().map_err(|_| MediaError::NoPlayer)
}

fn win_err(e: windows::core::Error) -> MediaError {
    MediaError::Backend(e.to_string())
}

/// Fold the current SMTC session and its live capabilities into a snapshot.
/// `Ok(None)` when no app owns the media session.
fn snapshot_with_caps_blocking() -> Result<Option<MediaSnapshot>, MediaError> {
    let mgr = manager().map_err(|error| manager_unavailable(&error.to_string()))?;
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
    let flags = info.Controls().ok().map(|c| ControlFlags {
        next: c.IsNextEnabled().ok(), // best-effort: optional read; absence is a normal condition
        previous: c.IsPreviousEnabled().ok(), // best-effort: optional read; absence is a normal condition
        position: c.IsPlaybackPositionEnabled().ok(), // best-effort: optional read; absence is a normal condition
        shuffle: c.IsShuffleEnabled().ok(), // best-effort: optional read; absence is a normal condition
        repeat: c.IsRepeatEnabled().ok(), // best-effort: optional read; absence is a normal condition
    });
    let caps = session_caps(flags);

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

    let (position, length) = session
        .GetTimelineProperties()
        .ok() // best-effort: optional read; absence is a normal condition
        .map(|t| {
            timeline(
                t.Position().ok().map(|ts| ts.Duration),
                t.EndTime().ok().map(|ts| ts.Duration),
            )
        })
        .unwrap_or((None, None));

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

    let state = MediaState {
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
        can_go_next: caps.can_go_next,
        can_go_previous: caps.can_go_previous,
        art_url: None, // thumbnail is a stream, not fetched in v1
        kind,
        can_seek: caps.can_seek,
        track_id: None,
    };

    Ok(Some(MediaSnapshot {
        state,
        caps: snapshot_caps(MediaBackend::caps(&Smtc), caps),
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

#[cfg(test)]
mod tests {
    use super::session_if_present;

    #[test]
    fn missing_current_session_is_a_healthy_empty_result() {
        assert!(session_if_present::<(), _>(Err("no current session")).is_none());
    }
}
