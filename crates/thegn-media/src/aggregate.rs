//! Multi-source aggregator — runs several boxed [`MediaBackend`] sources at once and
//! surfaces whichever is actually playing. This is what makes `auto` on Linux
//! cover *all* common players out of the box: MPRIS (browsers/Spotify/VLC/mpv
//! -with-`mpv-mpris`), native MPD (mpd/mpc/rmpc/ncmpcpp), and a live mpv IPC
//! socket, composed into one badge.
//!
//! Selection reuses the same five-tier precedence the MPRIS picker uses
//! (`players_priority` → sticky-active → first-playing → sticky-present →
//! first-present), generalized across sources on the player name. The push
//! [`AggregateWatch`] polls every child's own signal stream (MPRIS D-Bus signals,
//! MPD `idle`) concurrently, so the ~0%-idle contract holds — poll-only children
//! just contribute nothing to the stream and ride the host's safety poll.

use std::collections::HashSet;
use std::sync::Mutex;

use futures::future::{BoxFuture, join_all, select_all};

use crate::model::{MediaState, PlaybackState};
use crate::{MediaBackend, MediaCaps, MediaClient, MediaError, MediaSnapshot, MediaWatch};

/// A child that does not answer a snapshot cannot hold the aggregate hostage.
/// Child futures are polled concurrently, while results are consumed in child
/// order so source priority remains deterministic.
const SNAPSHOT_BUDGET: std::time::Duration = std::time::Duration::from_secs(2);

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
}

impl MediaBackend for Aggregate {
    fn snapshot(&self) -> BoxFuture<'_, Result<Option<MediaState>, MediaError>> {
        Box::pin(async move {
            self.snapshot_with_caps_inner()
                .await
                .map(|snapshot| snapshot.map(|snapshot| snapshot.state))
        })
    }

    fn snapshot_with_caps(&self) -> BoxFuture<'_, Result<Option<MediaSnapshot>, MediaError>> {
        Box::pin(async move { self.snapshot_with_caps_inner().await })
    }

    // Every control op delegates to the active child.
    fn play_pause(&self) -> BoxFuture<'_, Result<(), MediaError>> {
        Box::pin(async move { self.active_child()?.play_pause().await })
    }
    fn next(&self) -> BoxFuture<'_, Result<(), MediaError>> {
        Box::pin(async move { self.active_child()?.next().await })
    }
    fn previous(&self) -> BoxFuture<'_, Result<(), MediaError>> {
        Box::pin(async move { self.active_child()?.previous().await })
    }
    fn set_shuffle(&self, on: bool) -> BoxFuture<'_, Result<(), MediaError>> {
        Box::pin(async move { self.active_child()?.set_shuffle(on).await })
    }
    fn set_loop(&self, mode: crate::model::LoopMode) -> BoxFuture<'_, Result<(), MediaError>> {
        Box::pin(async move { self.active_child()?.set_loop(mode).await })
    }
    fn volume_step(&self, delta: f64) -> BoxFuture<'_, Result<(), MediaError>> {
        Box::pin(async move { self.active_child()?.volume_step(delta).await })
    }
    fn playlists(&self) -> BoxFuture<'_, Result<Vec<crate::model::Playlist>, MediaError>> {
        Box::pin(async move { self.active_child()?.playlists().await })
    }
    fn activate_playlist<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<(), MediaError>> {
        Box::pin(async move { self.active_child()?.activate_playlist(id).await })
    }
    fn seek(
        &self,
        offset: std::time::Duration,
        forward: bool,
    ) -> BoxFuture<'_, Result<(), MediaError>> {
        Box::pin(async move { self.active_child()?.seek(offset, forward).await })
    }
    fn set_position<'a>(
        &'a self,
        pos: std::time::Duration,
        track_id: Option<&'a str>,
    ) -> BoxFuture<'a, Result<(), MediaError>> {
        Box::pin(async move { self.active_child()?.set_position(pos, track_id).await })
    }
    fn set_volume(&self, level: u8) -> BoxFuture<'_, Result<(), MediaError>> {
        Box::pin(async move { self.active_child()?.set_volume(level).await })
    }
    fn queue(&self) -> BoxFuture<'_, Result<Vec<crate::model::QueueItem>, MediaError>> {
        Box::pin(async move { self.active_child()?.queue().await })
    }
    fn queue_page(&self) -> BoxFuture<'_, Result<crate::model::QueuePage, MediaError>> {
        Box::pin(async move { self.active_child()?.queue_page().await })
    }
    fn playlist_page(&self) -> BoxFuture<'_, Result<crate::model::PlaylistPage, MediaError>> {
        Box::pin(async move { self.active_child()?.playlist_page().await })
    }
    fn play_queue_item<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<(), MediaError>> {
        Box::pin(async move { self.active_child()?.play_queue_item(id).await })
    }
    fn chapter_next(&self) -> BoxFuture<'_, Result<(), MediaError>> {
        Box::pin(async move { self.active_child()?.chapter_next().await })
    }
    fn chapter_prev(&self) -> BoxFuture<'_, Result<(), MediaError>> {
        Box::pin(async move { self.active_child()?.chapter_prev().await })
    }
    fn toggle_fullscreen(&self) -> BoxFuture<'_, Result<(), MediaError>> {
        Box::pin(async move { self.active_child()?.toggle_fullscreen().await })
    }

    /// Union of every child's controllable players (for the picker), retaining
    /// one overflow name so the consumer can report truncation.
    fn players(&self) -> BoxFuture<'_, Result<Vec<String>, MediaError>> {
        Box::pin(async move {
            let players = join_all(self.children.iter().enumerate().map(|(i, c)| async move {
                (i, tokio::time::timeout(SNAPSHOT_BUDGET, c.players()).await)
            }))
            .await;
            let mut out = Vec::new();
            let mut seen = HashSet::new();
            let mut failures = Vec::new();
            for (i, result) in players {
                let source_players = match result {
                    Ok(Ok(source_players)) => source_players,
                    Ok(Err(error)) => {
                        tracing::debug!(target: "thegn::media", child = i, %error, "media player listing failed");
                        failures.push(error);
                        continue;
                    }
                    Err(error) => {
                        tracing::debug!(target: "thegn::media", child = i, %error, "media players timed out");
                        failures.push(MediaError::Unavailable("player listing timed out".into()));
                        continue;
                    }
                };
                for p in source_players {
                    if out.len() == crate::model::PLAYER_LIST_LIMIT {
                        break;
                    }
                    if seen.insert(p.clone()) {
                        out.push(p);
                    }
                }
            }
            if out.is_empty() && !self.children.is_empty() && failures.len() == self.children.len()
            {
                Err(failures.remove(0))
            } else {
                Ok(out)
            }
        })
    }

    /// Fan every child that offers a push watcher into one stream. `None` when no
    /// child pushes (the host then falls back to its poll ticker).
    fn watch(&self) -> BoxFuture<'_, Result<Option<Box<dyn MediaWatch + Send>>, MediaError>> {
        Box::pin(async move {
            let watches = join_all(
                self.children
                    .iter()
                    .enumerate()
                    .map(|(i, c)| bounded_watch(i, c.watch(), SNAPSHOT_BUDGET)),
            )
            .await;
            let mut children = Vec::new();
            let mut failures = Vec::new();
            let mut non_failures = 0;
            for result in watches {
                match result {
                    Ok(Some(watch)) => {
                        non_failures += 1;
                        children.push(watch);
                    }
                    Ok(None) => non_failures += 1,
                    Err(error) => failures.push(error),
                }
            }
            if !children.is_empty() {
                Ok(Some(
                    Box::new(AggregateWatch { children }) as Box<dyn MediaWatch + Send>
                ))
            } else if !self.children.is_empty() && non_failures == 0 {
                Err(failures.remove(0))
            } else {
                Ok(None)
            }
        })
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
    // Gather both state and caps from each child under one deadline. Keeping
    // them together preserves per-snapshot capabilities through aggregation.
    async fn snapshot_with_caps_inner(&self) -> Result<Option<MediaSnapshot>, MediaError> {
        // Gather independently under the same deadline. `join_all` retains
        // child order even when completions arrive out of order.
        let snapshots = join_all(self.children.iter().enumerate().map(|(i, c)| async move {
            let result = tokio::time::timeout(SNAPSHOT_BUDGET, c.snapshot_with_caps()).await;
            (i, result)
        }))
        .await;
        let mut cands: Vec<(usize, MediaSnapshot)> = Vec::new();
        let mut failures: Vec<(usize, MediaError)> = Vec::new();
        for (i, result) in snapshots {
            match result {
                Ok(Ok(Some(snapshot))) => cands.push((i, snapshot)),
                Ok(Ok(None)) | Ok(Err(MediaError::NoPlayer)) => {}
                Ok(Err(error)) => failures.push((i, error)),
                Err(_) => {
                    failures.push((i, MediaError::Unavailable("media source timed out".into())))
                }
            }
        }
        if cands.is_empty() {
            if !failures.is_empty() {
                let active = *self.active.lock().unwrap();
                let chosen = active
                    .and_then(|active| failures.iter().position(|(idx, _)| *idx == active))
                    .unwrap_or(0);
                return Err(failures.swap_remove(chosen).1);
            }
            *self.active.lock().unwrap() = None;
            return Ok(None);
        }
        for (child, error) in &failures {
            tracing::debug!(target: "thegn::media", child = *child, %error, "media snapshot failed; using another source");
        }
        let sticky = self.sticky.lock().unwrap().clone();
        let view: Vec<(&str, PlaybackState)> = cands
            .iter()
            .map(|(_, snapshot)| (snapshot.state.player.as_str(), snapshot.state.state))
            .collect();
        let pick = choose_state(&view, &self.priority, sticky.as_deref()).unwrap_or(0);
        let (child_idx, chosen) = cands.swap_remove(pick);
        *self.active.lock().unwrap() = Some(child_idx);
        *self.sticky.lock().unwrap() = Some(chosen.state.player.clone());
        Ok(Some(chosen))
    }
}

fn bounded_watch<'a>(
    child: usize,
    watch: BoxFuture<'a, Result<Option<Box<dyn MediaWatch + Send>>, MediaError>>,
    budget: std::time::Duration,
) -> BoxFuture<'a, Result<Option<Box<dyn MediaWatch + Send>>, MediaError>> {
    Box::pin(async move {
        match tokio::time::timeout(budget, watch).await {
            Ok(watch) => watch,
            Err(error) => {
                tracing::debug!(target: "thegn::media", child, %error, "media watcher setup timed out");
                Err(MediaError::Unavailable("watcher setup timed out".into()))
            }
        }
    })
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
/// in child order). Mirrors `mpris`'s five-tier precedence, generalized
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

/// A merged push stream that directly owns every child watcher. Child futures
/// are polled concurrently; dropping this value drops all watchers immediately.
/// The merged stream ends as soon as any child stream ends, so the host can
/// reconnect and resolve the available sources again.
pub struct AggregateWatch {
    children: Vec<Box<dyn MediaWatch + Send>>,
}

impl MediaWatch for AggregateWatch {
    fn changed(
        &mut self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + '_>> {
        Box::pin(async move {
            let futures = self
                .children
                .iter_mut()
                .enumerate()
                .map(|(index, watch)| {
                    Box::pin(async move { (index, watch.changed().await) })
                        as BoxFuture<'_, (usize, bool)>
                })
                .collect::<Vec<_>>();
            if futures.is_empty() {
                return false;
            }
            let ((_, changed), _, remaining) = select_all(futures).await;
            drop(remaining);
            changed
        })
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

    #[tokio::test]
    async fn dropping_aggregate_watch_drops_children_immediately() {
        let dropped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let aggregate = Aggregate::new(
            vec![fake_backend(Box::new(DropWatch(dropped.clone())))],
            Vec::new(),
        );
        let watch = aggregate.watch().await.unwrap().unwrap();
        drop(watch);
        assert!(dropped.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[tokio::test]
    async fn aborting_aggregate_changed_drops_children() {
        let dropped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let aggregate = Aggregate::new(
            vec![fake_backend(Box::new(DropWatch(dropped.clone())))],
            Vec::new(),
        );
        let watch = aggregate.watch().await.unwrap().unwrap();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let mut watch = watch;
            let changed = watch.changed();
            started_tx.send(()).unwrap();
            changed.await
        });
        started_rx.await.unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(dropped.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[tokio::test]
    async fn healthy_child_wakes_while_stalled_child_stays_owned() {
        let stalled_dropped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let aggregate = Aggregate::new(
            vec![
                fake_backend(Box::new(DropWatch(stalled_dropped.clone()))),
                fake_backend(Box::new(ReadyWatch(Some(true)))),
            ],
            Vec::new(),
        );
        let mut watch = aggregate.watch().await.unwrap().unwrap();
        assert!(watch.changed().await);
        assert!(!stalled_dropped.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[tokio::test]
    async fn aggregate_watch_ends_when_all_children_end() {
        let aggregate = Aggregate::new(
            vec![fake_backend(Box::new(ReadyWatch(Some(false))))],
            Vec::new(),
        );
        let mut watch = aggregate.watch().await.unwrap().unwrap();
        assert!(!watch.changed().await);
    }

    #[tokio::test]
    async fn aggregate_watch_ends_when_any_child_ends() {
        let stalled_dropped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let aggregate = Aggregate::new(
            vec![
                fake_backend(Box::new(DropWatch(stalled_dropped.clone()))),
                fake_backend(Box::new(ReadyWatch(Some(false)))),
            ],
            Vec::new(),
        );
        let mut watch = aggregate.watch().await.unwrap().unwrap();

        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(1), watch.changed()).await,
            Ok(false)
        );
        drop(watch);
        assert!(stalled_dropped.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[tokio::test]
    async fn aggregate_watch_reports_simultaneous_changes_as_refresh_wakes() {
        let aggregate = Aggregate::new(
            vec![
                fake_backend(Box::new(WakeOnceWatch(false))),
                fake_backend(Box::new(WakeOnceWatch(false))),
            ],
            Vec::new(),
        );
        let mut watch = aggregate.watch().await.unwrap().unwrap();
        // Each child produces a wake, then remains alive and pending.
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(1), watch.changed()).await,
            Ok(true)
        );
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(1), watch.changed()).await,
            Ok(true)
        );
    }

    #[tokio::test]
    async fn stalled_child_watch_does_not_hold_up_healthy_watchers() {
        let healthy = Box::pin(async {
            Ok::<_, MediaError>(Some(Box::new(TestWatch) as Box<dyn MediaWatch + Send>))
        });
        let stalled = Box::pin(std::future::pending());
        let watches = join_all([
            bounded_watch(0, stalled, std::time::Duration::from_millis(10)),
            bounded_watch(1, healthy, std::time::Duration::from_millis(10)),
        ])
        .await;

        assert!(
            watches[0].is_err(),
            "a stalled watcher should report timeout"
        );
        assert!(
            watches[1].as_ref().is_ok_and(Option::is_some),
            "a healthy watcher should be retained"
        );
    }

    #[tokio::test]
    async fn typed_players_union_skips_a_failed_child() {
        let aggregate = Aggregate::new(
            vec![
                players_backend(Ok(vec!["a".into()]), Ok(None)),
                players_backend(Err(MediaError::Unavailable("bus".into())), Ok(None)),
            ],
            Vec::new(),
        );
        assert_eq!(aggregate.players().await, Ok(vec!["a".into()]));
    }

    #[tokio::test]
    async fn typed_players_error_when_every_child_fails() {
        let aggregate = Aggregate::new(
            vec![
                players_backend(Err(MediaError::Unavailable("first".into())), Ok(None)),
                players_backend(Err(MediaError::Unavailable("second".into())), Ok(None)),
            ],
            Vec::new(),
        );
        assert!(aggregate.players().await.is_err());
    }

    #[tokio::test]
    async fn players_union_is_deduplicated_in_order_and_capped() {
        // Positive control: ordered deduplication also passes on the uncapped implementation.
        let aggregate = Aggregate::new(
            vec![
                players_backend(Ok(vec!["a".into(), "b".into()]), Ok(None)),
                players_backend(Ok(vec!["b".into(), "c".into()]), Ok(None)),
            ],
            Vec::new(),
        );
        assert_eq!(
            aggregate.players().await,
            Ok(vec!["a".into(), "b".into(), "c".into()])
        );

        let first_players = (0..crate::model::MAX_PLAYERS + 5)
            .map(|index| format!("player-{index}"))
            .collect::<Vec<_>>();
        let expected = first_players[..crate::model::PLAYER_LIST_LIMIT].to_vec();
        let aggregate = Aggregate::new(
            vec![
                players_backend(Ok(first_players), Ok(None)),
                players_backend(Ok(vec!["after-cap".into()]), Ok(None)),
            ],
            Vec::new(),
        );
        assert_eq!(aggregate.players().await, Ok(expected));
    }

    #[tokio::test]
    async fn players_union_keeps_one_overflow_name() {
        let first_players = (0..crate::model::MAX_PLAYERS)
            .map(|index| format!("player-{index}"))
            .collect::<Vec<_>>();
        let mut expected = first_players.clone();
        expected.push("extra".into());
        let aggregate = Aggregate::new(
            vec![
                players_backend(Ok(first_players), Ok(None)),
                players_backend(Ok(vec!["extra".into(), "more".into()]), Ok(None)),
            ],
            Vec::new(),
        );

        assert_eq!(aggregate.players().await, Ok(expected));
    }

    #[tokio::test]
    async fn typed_watch_error_when_every_child_fails() {
        let aggregate = Aggregate::new(
            vec![
                players_backend(Ok(Vec::new()), Err(MediaError::Unavailable("one".into()))),
                players_backend(Ok(Vec::new()), Err(MediaError::Unavailable("two".into()))),
            ],
            Vec::new(),
        );
        assert!(aggregate.watch().await.is_err());
    }

    #[tokio::test]
    async fn typed_watch_ok_none_when_no_child_pushes() {
        let aggregate = Aggregate::new(
            vec![
                players_backend(Ok(Vec::new()), Ok(None)),
                players_backend(Ok(Vec::new()), Ok(None)),
            ],
            Vec::new(),
        );
        assert!(aggregate.watch().await.unwrap().is_none());
    }

    fn players_backend(
        players: Result<Vec<String>, MediaError>,
        watch: Result<Option<Box<dyn MediaWatch + Send>>, MediaError>,
    ) -> MediaClient {
        Box::new(PlayersBackend {
            players,
            watch: Mutex::new(Some(watch)),
        })
    }

    type PendingWatch = Option<Result<Option<Box<dyn MediaWatch + Send>>, MediaError>>;

    struct PlayersBackend {
        players: Result<Vec<String>, MediaError>,
        watch: Mutex<PendingWatch>,
    }

    impl MediaBackend for PlayersBackend {
        fn snapshot(&self) -> BoxFuture<'_, Result<Option<MediaState>, MediaError>> {
            no_player()
        }
        fn play_pause(&self) -> BoxFuture<'_, Result<(), MediaError>> {
            no_player()
        }
        fn next(&self) -> BoxFuture<'_, Result<(), MediaError>> {
            no_player()
        }
        fn previous(&self) -> BoxFuture<'_, Result<(), MediaError>> {
            no_player()
        }
        fn set_shuffle(&self, _on: bool) -> BoxFuture<'_, Result<(), MediaError>> {
            no_player()
        }
        fn set_loop(&self, _mode: crate::model::LoopMode) -> BoxFuture<'_, Result<(), MediaError>> {
            no_player()
        }
        fn volume_step(&self, _delta: f64) -> BoxFuture<'_, Result<(), MediaError>> {
            no_player()
        }
        fn playlists(&self) -> BoxFuture<'_, Result<Vec<crate::model::Playlist>, MediaError>> {
            no_player()
        }
        fn activate_playlist<'a>(&'a self, _id: &'a str) -> BoxFuture<'a, Result<(), MediaError>> {
            no_player()
        }

        fn caps(&self) -> MediaCaps {
            NO_CAPS
        }

        fn players(&self) -> BoxFuture<'_, Result<Vec<String>, MediaError>> {
            Box::pin(async move { self.players.clone() })
        }

        fn watch(&self) -> BoxFuture<'_, Result<Option<Box<dyn MediaWatch + Send>>, MediaError>> {
            Box::pin(async move { self.watch.lock().unwrap().take().unwrap_or(Ok(None)) })
        }
    }

    #[tokio::test]
    async fn snapshot_error_is_not_reported_as_empty() {
        let aggregate = Aggregate::new(
            vec![
                snapshot_backend(
                    Err(MediaError::Backend("first failed".into())),
                    NO_CAPS,
                    None,
                ),
                snapshot_backend(
                    Err(MediaError::Backend("active failed".into())),
                    NO_CAPS,
                    None,
                ),
                snapshot_backend(Ok(None), NO_CAPS, None),
            ],
            Vec::new(),
        );
        *aggregate.active.lock().unwrap() = Some(1);
        *aggregate.sticky.lock().unwrap() = Some("sticky".into());

        assert_eq!(
            aggregate.snapshot().await,
            Err(MediaError::Backend("active failed".into()))
        );
        assert_eq!(*aggregate.active.lock().unwrap(), Some(1));
        assert_eq!(*aggregate.sticky.lock().unwrap(), Some("sticky".into()));
    }

    #[tokio::test]
    async fn no_player_and_empty_children_are_healthy_empty_results() {
        let aggregate = Aggregate::new(
            vec![
                snapshot_backend(Ok(None), NO_CAPS, None),
                snapshot_backend(Err(MediaError::NoPlayer), NO_CAPS, None),
            ],
            Vec::new(),
        );

        assert_eq!(aggregate.snapshot().await.unwrap(), None);
    }

    #[tokio::test]
    async fn snapshot_with_caps_preserves_the_chosen_child_caps() {
        let state = MediaState {
            player: "preferred".into(),
            state: PLAYING,
            ..MediaState::default()
        };
        let advertised = MediaCaps {
            volume: true,
            ..NO_CAPS
        };
        let aggregate = Aggregate::new(
            vec![
                snapshot_backend(
                    Ok(Some(MediaState {
                        player: "other".into(),
                        state: PLAYING,
                        ..MediaState::default()
                    })),
                    NO_CAPS,
                    None,
                ),
                snapshot_backend(Ok(Some(state.clone())), NO_CAPS, Some(advertised)),
            ],
            vec!["preferred".into()],
        );

        let snapshot = aggregate.snapshot_with_caps().await.unwrap().unwrap();
        assert_eq!(snapshot.state, state);
        assert_eq!(snapshot.caps, advertised);
    }

    #[tokio::test]
    async fn queue_page_is_forwarded_to_the_active_child() {
        let page = crate::model::QueuePage {
            items: vec![crate::model::QueueItem {
                id: "row".into(),
                ..Default::default()
            }],
            total: 900,
        };
        let aggregate = Aggregate::new(
            vec![snapshot_backend_with_queue_page(
                Ok(Some(MediaState {
                    player: "active".into(),
                    state: PLAYING,
                    ..Default::default()
                })),
                NO_CAPS,
                page.clone(),
            )],
            Vec::new(),
        );

        aggregate.snapshot().await.unwrap();
        assert_eq!(aggregate.queue_page().await.unwrap().total, 900);
        assert_eq!(aggregate.queue_page().await.unwrap().items, page.items);
    }

    #[tokio::test]
    async fn playlist_page_is_forwarded_to_the_active_child() {
        let page = crate::model::PlaylistPage {
            items: vec![crate::model::Playlist {
                id: "playlist-id".into(),
                name: "Favorites".into(),
            }],
            total: 250,
        };
        let aggregate = Aggregate::new(
            vec![snapshot_backend_with_playlist_page(
                Ok(Some(MediaState {
                    player: "active".into(),
                    state: PLAYING,
                    ..Default::default()
                })),
                NO_CAPS,
                page.clone(),
            )],
            Vec::new(),
        );

        aggregate.snapshot().await.unwrap();
        assert_eq!(aggregate.playlist_page().await.unwrap(), page);
    }

    struct SnapshotBackend {
        result: Result<Option<MediaState>, MediaError>,
        caps: MediaCaps,
        snapshot_caps: Option<MediaCaps>,
        queue_page: Option<crate::model::QueuePage>,
        playlist_page: Option<crate::model::PlaylistPage>,
    }

    fn snapshot_backend(
        result: Result<Option<MediaState>, MediaError>,
        caps: MediaCaps,
        snapshot_caps: Option<MediaCaps>,
    ) -> MediaClient {
        Box::new(SnapshotBackend {
            result,
            caps,
            snapshot_caps,
            queue_page: None,
            playlist_page: None,
        })
    }

    fn snapshot_backend_with_queue_page(
        result: Result<Option<MediaState>, MediaError>,
        caps: MediaCaps,
        queue_page: crate::model::QueuePage,
    ) -> MediaClient {
        Box::new(SnapshotBackend {
            result,
            caps,
            snapshot_caps: None,
            queue_page: Some(queue_page),
            playlist_page: None,
        })
    }

    fn snapshot_backend_with_playlist_page(
        result: Result<Option<MediaState>, MediaError>,
        caps: MediaCaps,
        playlist_page: crate::model::PlaylistPage,
    ) -> MediaClient {
        Box::new(SnapshotBackend {
            result,
            caps,
            snapshot_caps: None,
            queue_page: None,
            playlist_page: Some(playlist_page),
        })
    }

    impl MediaBackend for SnapshotBackend {
        fn snapshot(&self) -> BoxFuture<'_, Result<Option<MediaState>, MediaError>> {
            Box::pin(async move { self.result.clone() })
        }

        fn snapshot_with_caps(&self) -> BoxFuture<'_, Result<Option<MediaSnapshot>, MediaError>> {
            Box::pin(async move {
                self.result.clone().map(|state| {
                    state.map(|state| MediaSnapshot {
                        state,
                        caps: self.snapshot_caps.unwrap_or(self.caps),
                    })
                })
            })
        }

        fn play_pause(&self) -> BoxFuture<'_, Result<(), MediaError>> {
            no_player()
        }
        fn next(&self) -> BoxFuture<'_, Result<(), MediaError>> {
            no_player()
        }
        fn previous(&self) -> BoxFuture<'_, Result<(), MediaError>> {
            no_player()
        }
        fn set_shuffle(&self, _on: bool) -> BoxFuture<'_, Result<(), MediaError>> {
            no_player()
        }
        fn set_loop(&self, _mode: crate::model::LoopMode) -> BoxFuture<'_, Result<(), MediaError>> {
            no_player()
        }
        fn volume_step(&self, _delta: f64) -> BoxFuture<'_, Result<(), MediaError>> {
            no_player()
        }
        fn playlists(&self) -> BoxFuture<'_, Result<Vec<crate::model::Playlist>, MediaError>> {
            no_player()
        }
        fn activate_playlist<'a>(&'a self, _id: &'a str) -> BoxFuture<'a, Result<(), MediaError>> {
            no_player()
        }
        fn caps(&self) -> MediaCaps {
            self.caps
        }
        fn players(&self) -> BoxFuture<'_, Result<Vec<String>, MediaError>> {
            Box::pin(async { Ok(Vec::new()) })
        }

        fn queue_page(&self) -> BoxFuture<'_, Result<crate::model::QueuePage, MediaError>> {
            Box::pin(async move { Ok(self.queue_page.clone().unwrap_or_default()) })
        }

        fn playlist_page(&self) -> BoxFuture<'_, Result<crate::model::PlaylistPage, MediaError>> {
            Box::pin(async move { Ok(self.playlist_page.clone().unwrap_or_default()) })
        }
    }

    struct TestWatch;

    impl MediaWatch for TestWatch {
        fn changed(
            &mut self,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + '_>> {
            Box::pin(std::future::pending())
        }
    }

    struct DropWatch(std::sync::Arc<std::sync::atomic::AtomicBool>);

    impl Drop for DropWatch {
        fn drop(&mut self) {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    impl MediaWatch for DropWatch {
        fn changed(
            &mut self,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + '_>> {
            Box::pin(std::future::pending())
        }
    }

    struct ReadyWatch(Option<bool>);

    impl MediaWatch for ReadyWatch {
        fn changed(
            &mut self,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + '_>> {
            Box::pin(async move { self.0.take().unwrap_or(false) })
        }
    }

    struct WakeOnceWatch(bool);

    impl MediaWatch for WakeOnceWatch {
        fn changed(
            &mut self,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + '_>> {
            if self.0 {
                Box::pin(std::future::pending())
            } else {
                self.0 = true;
                Box::pin(async { true })
            }
        }
    }

    struct FakeBackend {
        watch: std::sync::Mutex<Option<Box<dyn MediaWatch + Send>>>,
    }

    fn fake_backend(watch: Box<dyn MediaWatch + Send>) -> MediaClient {
        Box::new(FakeBackend {
            watch: std::sync::Mutex::new(Some(watch)),
        })
    }

    fn no_player<'a, T: Send + 'a>() -> BoxFuture<'a, Result<T, MediaError>> {
        Box::pin(async { Err(MediaError::NoPlayer) })
    }

    impl MediaBackend for FakeBackend {
        fn snapshot(&self) -> BoxFuture<'_, Result<Option<MediaState>, MediaError>> {
            no_player()
        }

        fn play_pause(&self) -> BoxFuture<'_, Result<(), MediaError>> {
            no_player()
        }

        fn next(&self) -> BoxFuture<'_, Result<(), MediaError>> {
            no_player()
        }

        fn previous(&self) -> BoxFuture<'_, Result<(), MediaError>> {
            no_player()
        }

        fn set_shuffle(&self, _on: bool) -> BoxFuture<'_, Result<(), MediaError>> {
            no_player()
        }

        fn set_loop(&self, _mode: crate::model::LoopMode) -> BoxFuture<'_, Result<(), MediaError>> {
            no_player()
        }

        fn volume_step(&self, _delta: f64) -> BoxFuture<'_, Result<(), MediaError>> {
            no_player()
        }

        fn playlists(&self) -> BoxFuture<'_, Result<Vec<crate::model::Playlist>, MediaError>> {
            no_player()
        }

        fn activate_playlist<'a>(&'a self, _id: &'a str) -> BoxFuture<'a, Result<(), MediaError>> {
            no_player()
        }

        fn caps(&self) -> MediaCaps {
            NO_CAPS
        }

        fn watch(&self) -> BoxFuture<'_, Result<Option<Box<dyn MediaWatch + Send>>, MediaError>> {
            Box::pin(async move { Ok(self.watch.lock().unwrap().take()) })
        }

        fn players(&self) -> BoxFuture<'_, Result<Vec<String>, MediaError>> {
            Box::pin(async { Ok(Vec::new()) })
        }
    }
}
