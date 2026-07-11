//! Multi-source aggregator — runs several [`MediaClient`] sources at once and
//! surfaces whichever is actually playing. This is what makes `auto` on Linux
//! cover *all* common players out of the box: MPRIS (browsers/Spotify/VLC/mpv
//! -with-`mpv-mpris`), native MPD (mpd/mpc/rmpc/ncmpcpp), and a live mpv IPC
//! socket, composed into one badge.
//!
//! Selection reuses the same five-tier precedence the MPRIS picker uses
//! (`players_priority` → sticky-active → first-playing → sticky-present →
//! first-present), generalized across sources on the player name. The push
//! [`AggregateWatch`] fans every child's own signal stream (MPRIS D-Bus signals,
//! MPD `idle`) into one, so the ~0%-idle contract holds — poll-only children just
//! contribute nothing to the stream and ride the host's safety poll.

use std::sync::Mutex;

use crate::model::{MediaState, PlaybackState};
use crate::{MediaBackend, MediaCaps, MediaClient, MediaError, MediaWatch};

/// A set of media sources presented as one backend.
pub struct Aggregate {
    children: Vec<MediaClient>,
    priority: Vec<String>,
    /// The player name last shown, so the badge doesn't flip between two
    /// simultaneously-playing sources on every signal.
    sticky: Mutex<Option<String>>,
    /// Index of the child that produced the current snapshot — control ops
    /// (play/pause, next, …) target it.
    active: Mutex<Option<usize>>,
}

impl Aggregate {
    pub fn new(children: Vec<MediaClient>, priority: Vec<String>) -> Aggregate {
        Aggregate {
            children,
            priority,
            sticky: Mutex::new(None),
            active: Mutex::new(None),
        }
    }

    /// Union of every child's controllable players (for the picker).
    pub async fn players(&self) -> Vec<String> {
        let mut out = Vec::new();
        for c in &self.children {
            // Boxed: `MediaClient::players` → here → `MediaClient::players` is a
            // recursive async cycle (the enum contains `Aggregate`).
            for p in Box::pin(c.players()).await {
                if !out.contains(&p) {
                    out.push(p);
                }
            }
        }
        out
    }

    /// Fan every child that offers a push watcher into one stream. `None` when no
    /// child pushes (the host then falls back to its poll ticker).
    pub async fn watch(&self) -> Option<Box<dyn MediaWatch + Send>> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<()>();
        let mut any = false;
        for c in &self.children {
            if let Some(mut w) = Box::pin(c.watch()).await {
                any = true;
                let tx = tx.clone();
                // Each child's watcher is driven on its own task; a tick on any
                // of them wakes the aggregate. Tasks end when their stream ends
                // or the receiver drops.
                tokio::spawn(async move {
                    while w.changed().await {
                        if tx.send(()).is_err() {
                            break;
                        }
                    }
                });
            }
        }
        any.then(|| Box::new(AggregateWatch { rx }) as Box<dyn MediaWatch + Send>)
    }
}

impl MediaBackend for Aggregate {
    async fn snapshot(&self) -> Result<Option<MediaState>, MediaError> {
        // Gather each child's best snapshot (skip failures/empties).
        let mut cands: Vec<(usize, MediaState)> = Vec::new();
        for (i, c) in self.children.iter().enumerate() {
            // Boxed to break the recursive async cycle (see `players`).
            if let Ok(Some(s)) = Box::pin(c.snapshot()).await {
                cands.push((i, s));
            }
        }
        if cands.is_empty() {
            *self.active.lock().unwrap() = None;
            return Ok(None);
        }
        let sticky = self.sticky.lock().unwrap().clone();
        let view: Vec<(&str, PlaybackState)> = cands
            .iter()
            .map(|(_, s)| (s.player.as_str(), s.state))
            .collect();
        let pick = choose_state(&view, &self.priority, sticky.as_deref()).unwrap_or(0);
        let (child_idx, chosen) = cands.swap_remove(pick);
        *self.active.lock().unwrap() = Some(child_idx);
        *self.sticky.lock().unwrap() = Some(chosen.player.clone());
        Ok(Some(chosen))
    }

    // Every control op delegates to the active child, boxed to break the
    // recursive async cycle (`MediaClient` contains `Aggregate`).
    async fn play_pause(&self) -> Result<(), MediaError> {
        Box::pin(self.active_child()?.play_pause()).await
    }
    async fn next(&self) -> Result<(), MediaError> {
        Box::pin(self.active_child()?.next()).await
    }
    async fn previous(&self) -> Result<(), MediaError> {
        Box::pin(self.active_child()?.previous()).await
    }
    async fn set_shuffle(&self, on: bool) -> Result<(), MediaError> {
        Box::pin(self.active_child()?.set_shuffle(on)).await
    }
    async fn set_loop(&self, mode: crate::model::LoopMode) -> Result<(), MediaError> {
        Box::pin(self.active_child()?.set_loop(mode)).await
    }
    async fn volume_step(&self, delta: f64) -> Result<(), MediaError> {
        Box::pin(self.active_child()?.volume_step(delta)).await
    }
    async fn playlists(&self) -> Result<Vec<crate::model::Playlist>, MediaError> {
        Box::pin(self.active_child()?.playlists()).await
    }
    async fn activate_playlist(&self, id: &str) -> Result<(), MediaError> {
        Box::pin(self.active_child()?.activate_playlist(id)).await
    }
    async fn seek(&self, offset: std::time::Duration, forward: bool) -> Result<(), MediaError> {
        Box::pin(self.active_child()?.seek(offset, forward)).await
    }
    async fn set_position(
        &self,
        pos: std::time::Duration,
        track_id: Option<&str>,
    ) -> Result<(), MediaError> {
        Box::pin(self.active_child()?.set_position(pos, track_id)).await
    }
    async fn set_volume(&self, level: u8) -> Result<(), MediaError> {
        Box::pin(self.active_child()?.set_volume(level)).await
    }
    async fn queue(&self) -> Result<Vec<crate::model::QueueItem>, MediaError> {
        Box::pin(self.active_child()?.queue()).await
    }
    async fn play_queue_item(&self, id: &str) -> Result<(), MediaError> {
        Box::pin(self.active_child()?.play_queue_item(id)).await
    }
    async fn chapter_next(&self) -> Result<(), MediaError> {
        Box::pin(self.active_child()?.chapter_next()).await
    }
    async fn chapter_prev(&self) -> Result<(), MediaError> {
        Box::pin(self.active_child()?.chapter_prev()).await
    }
    async fn toggle_fullscreen(&self) -> Result<(), MediaError> {
        Box::pin(self.active_child()?.toggle_fullscreen()).await
    }

    fn caps(&self) -> MediaCaps {
        // The active child's caps drive the UI; before the first snapshot fall
        // back to the first child's.
        let idx = self.active.lock().unwrap().or(Some(0));
        idx.and_then(|i| self.children.get(i))
            .map(|c| c.caps())
            .unwrap_or(NO_CAPS)
    }
}

impl Aggregate {
    /// The child that produced the current snapshot (control target). Falls back
    /// to the first child before the first snapshot.
    fn active_child(&self) -> Result<&MediaClient, MediaError> {
        let idx = self.active.lock().unwrap().unwrap_or(0);
        self.children.get(idx).ok_or(MediaError::NoPlayer)
    }
}

/// Everything-off capabilities, used only before any source has a player.
const NO_CAPS: MediaCaps = MediaCaps {
    shuffle: false,
    loop_mode: false,
    volume: false,
    playlists: false,
    signals: true, // the aggregate itself may push
    seek: false,
    art: false,
    queue: false,
    abs_volume: false,
    chapters: false,
    fullscreen: false,
};

/// Pick which source to show from the present `candidates` (each `(player, state)`
/// in child order). Mirrors [`crate::mpris`]'s five-tier precedence, generalized
/// across sources on the player name:
///
/// 1. **priority** — the first whose player equals or contains a configured
///    `players_priority` entry;
/// 2. **sticky-active** — keep the last-shown player while it stays playing/paused;
/// 3. **first playing** in child order;
/// 4. **sticky-present** — keep the last-shown player if still present at all
///    (rides out a brief inter-track gap);
/// 5. the first present candidate.
///
/// Returns the index into `candidates`, or `None` only when empty.
pub fn choose_state(
    candidates: &[(&str, PlaybackState)],
    priority: &[String],
    sticky: Option<&str>,
) -> Option<usize> {
    if candidates.is_empty() {
        return None;
    }
    for p in priority {
        if let Some(i) = candidates
            .iter()
            .position(|(name, _)| name == p || name.contains(p.as_str()))
        {
            return Some(i);
        }
    }
    if let Some(last) = sticky
        && let Some(i) = candidates
            .iter()
            .position(|(name, st)| *name == last && st.is_active())
    {
        return Some(i);
    }
    if let Some(i) = candidates
        .iter()
        .position(|(_, st)| matches!(st, PlaybackState::Playing))
    {
        return Some(i);
    }
    if let Some(last) = sticky
        && let Some(i) = candidates.iter().position(|(name, _)| *name == last)
    {
        return Some(i);
    }
    Some(0)
}

/// A merged push stream: fires whenever any child's watcher fires. Ends when
/// every child stream has ended (all sender tasks dropped).
pub struct AggregateWatch {
    rx: tokio::sync::mpsc::UnboundedReceiver<()>,
}

impl MediaWatch for AggregateWatch {
    fn changed(
        &mut self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + '_>> {
        Box::pin(async move { self.rx.recv().await.is_some() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLAYING: PlaybackState = PlaybackState::Playing;
    const PAUSED: PlaybackState = PlaybackState::Paused;
    const STOPPED: PlaybackState = PlaybackState::Stopped;

    #[test]
    fn priority_wins_across_sources() {
        let c = [("spotify", PAUSED), ("mpd", PLAYING)];
        let pri = vec!["mpd".to_string()];
        assert_eq!(choose_state(&c, &pri, None), Some(1));
    }

    #[test]
    fn first_playing_when_no_priority() {
        let c = [("mpd", PAUSED), ("spotify", PLAYING)];
        assert_eq!(choose_state(&c, &[], None), Some(1));
    }

    #[test]
    fn sticky_holds_while_active_even_if_other_also_plays() {
        // spotify was sticky and is still playing; don't jump to mpd.
        let c = [("mpd", PLAYING), ("spotify", PLAYING)];
        assert_eq!(choose_state(&c, &[], Some("spotify")), Some(1));
    }

    #[test]
    fn sticky_present_rides_inter_track_gap() {
        // Nothing playing; keep the sticky source rather than switching.
        let c = [("mpd", STOPPED), ("spotify", STOPPED)];
        assert_eq!(choose_state(&c, &[], Some("spotify")), Some(1));
    }

    #[test]
    fn falls_back_to_first_present() {
        let c = [("mpd", STOPPED), ("spotify", STOPPED)];
        assert_eq!(choose_state(&c, &[], None), Some(0));
        assert_eq!(choose_state(&[], &[], None), None);
    }
}
