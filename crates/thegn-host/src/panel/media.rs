//! Loop-owned state for the docked Media section.

use std::collections::HashSet;
use thegn_core::media::{MAX_QUEUE_ITEMS, MAX_SOURCES, QueuePage};
use thegn_core::media::{MediaCaps, MediaState, QueueItem};

use crate::media_art::ArtMosaic;

pub(crate) type MediaIdentity = (String, Option<String>);

/// A queue row and the player whose snapshot the queue was fetched for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct QueueItemLease {
    pub(crate) id: String,
    pub(crate) player: String,
}

/// Generation and snapshot identity attached to every asynchronous media
/// request. A matching track alone is not sufficient: it may disappear and
/// return while an older response is still in flight.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MediaRequest {
    pub(crate) generation: u64,
    pub(crate) watcher_generation: u64,
    pub(crate) identity: Option<MediaIdentity>,
}

#[derive(Debug)]
pub(crate) struct MediaQueueDelivery {
    pub(crate) request: MediaRequest,
    pub(crate) queue: Result<QueuePage, String>,
}

#[derive(Debug)]
pub(crate) struct MediaSourcesDelivery {
    pub(crate) request: MediaRequest,
    pub(crate) sources: Vec<String>,
}

/// Source/queue selection and identity-checked asynchronous decoration. Late
/// queue or art deliveries are ignored after a player/track change.
#[derive(Debug, Default)]
pub(crate) struct MediaPanelState {
    pub(crate) sources: Vec<String>,
    pub(crate) sources_truncated: bool,
    pub(crate) queue: Vec<QueueItem>,
    pub(crate) queue_truncated: usize,
    pub(crate) queue_error: Option<String>,
    pub(crate) art: Option<ArtMosaic>,
    pub(crate) health: crate::media_watch::MediaHealth,
    caps: Option<MediaCaps>,
    generation: u64,
    watcher_generation: u64,
    snapshot_identity: Option<MediaIdentity>,
    queue_identity: Option<MediaIdentity>,
    queue_request: Option<MediaRequest>,
    sources_stale: bool,
    art_identity: Option<String>,
    art_requested: Option<String>,
    art_enabled: bool,
}

impl MediaPanelState {
    fn identity(state: &MediaState) -> (String, Option<String>) {
        (
            state.player.clone(),
            state.track_id.clone().or_else(|| Some(state.now_playing())),
        )
    }

    pub(crate) fn sync_snapshot(
        &mut self,
        state: Option<&MediaState>,
        caps: Option<MediaCaps>,
        watcher_generation: u64,
    ) {
        let watcher_changed = self.watcher_generation != watcher_generation;
        if watcher_changed {
            // A config/player restart invalidates requests even when the old
            // snapshot is still displayed and compares equal to the next one.
            self.watcher_generation = watcher_generation;
            self.generation = self.generation.wrapping_add(1);
            self.clear_queue();
            self.queue_identity = None;
            self.queue_request = state
                .map(Self::identity)
                .map(|identity| self.request_for(Some(identity)));
            self.sources_stale = true;
        }
        let Some(state) = state else {
            self.sources.clear();
            self.sources_truncated = false;
            self.clear_queue();
            self.caps = None;
            if self.snapshot_identity.is_some() {
                self.generation = self.generation.wrapping_add(1);
            }
            self.snapshot_identity = None;
            self.art = None;
            self.queue_identity = None;
            self.queue_request = None;
            self.sources_stale = true;
            self.art_identity = None;
            self.art_requested = None;
            return;
        };
        self.caps = caps;
        let identity = Self::identity(state);
        if self.snapshot_identity.as_ref() != Some(&identity) {
            self.generation = self.generation.wrapping_add(1);
            self.snapshot_identity = Some(identity.clone());
            self.clear_queue();
            self.queue_identity = None;
            self.queue_request = Some(self.request_for(Some(identity)));
            self.sources_stale = true;
        }
        if self.art_identity.as_deref() != state.art_url.as_deref() {
            self.art = None;
            self.art_identity = None;
            self.art_requested = None;
        }
    }

    fn request_for(&self, identity: Option<MediaIdentity>) -> MediaRequest {
        MediaRequest {
            generation: self.generation,
            watcher_generation: self.watcher_generation,
            identity,
        }
    }

    pub(crate) fn begin_request(&mut self, state: Option<&MediaState>) {
        self.generation = self.generation.wrapping_add(1);
        self.snapshot_identity = state.map(Self::identity);
        self.clear_queue();
        self.queue_identity = None;
        self.queue_request = state
            .map(Self::identity)
            .map(|identity| self.request_for(Some(identity)));
        self.sources_stale = true;
        if let Some(state) = state
            && self.art_identity.as_deref() != state.art_url.as_deref()
        {
            self.art = None;
            self.art_identity = None;
            self.art_requested = None;
        }
        // An explicit panel open is a retry point after a failed fetch, while
        // repeated loop turns must not create duplicate in-flight decoders.
        self.art_requested = None;
    }

    pub(crate) fn cancel_art_request(&mut self) {
        self.art_requested = None;
    }

    pub(crate) fn take_queue_request(
        &mut self,
        state: Option<&MediaState>,
    ) -> Option<MediaRequest> {
        let request = self.queue_request.take()?;
        if request.generation == self.generation
            && request.watcher_generation == self.watcher_generation
            && request.identity.as_ref() == state.map(Self::identity).as_ref()
        {
            Some(request)
        } else {
            None
        }
    }

    pub(crate) fn take_sources_request(
        &mut self,
        state: Option<&MediaState>,
    ) -> Option<MediaRequest> {
        if !self.sources_stale {
            return None;
        }
        self.sources_stale = false;
        Some(self.request_for(state.map(Self::identity)))
    }

    /// Vec-shaped test entry point; delegates to `set_queue_page`.
    #[cfg(test)]
    pub(crate) fn set_queue(
        &mut self,
        request: MediaRequest,
        state: Option<&MediaState>,
        queue: Result<Vec<QueueItem>, String>,
    ) {
        self.set_queue_page(request, state, queue.map(QueuePage::from_rows));
    }

    pub(crate) fn set_queue_page(
        &mut self,
        request: MediaRequest,
        state: Option<&MediaState>,
        page: Result<QueuePage, String>,
    ) {
        let Some(state) = state else { return };
        if request.generation == self.generation
            && request.watcher_generation == self.watcher_generation
            && request.identity.as_ref() == Some(&Self::identity(state))
        {
            self.queue_identity = Some(Self::identity(state));
            match page {
                Ok(mut page) => {
                    self.queue_truncated =
                        page.hidden() + page.items.len().saturating_sub(MAX_QUEUE_ITEMS);
                    page.items.truncate(MAX_QUEUE_ITEMS);
                    self.queue = page.items;
                    self.queue_error = None;
                }
                Err(error) => {
                    self.queue.clear();
                    self.queue_truncated = 0;
                    self.queue_error = Some(error);
                }
            }
        }
    }

    fn clear_queue(&mut self) {
        self.queue.clear();
        self.queue_truncated = 0;
        self.queue_error = None;
    }

    pub(crate) fn set_sources(
        &mut self,
        request: MediaRequest,
        state: Option<&MediaState>,
        sources: Vec<String>,
    ) {
        if request.generation != self.generation
            || request.watcher_generation != self.watcher_generation
            || request.identity.as_ref() != state.map(Self::identity).as_ref()
        {
            return;
        }
        let mut unique = Vec::with_capacity(sources.len().min(MAX_SOURCES));
        let mut seen = HashSet::with_capacity(sources.len().min(MAX_SOURCES));
        self.sources_truncated = false;
        for source in &sources {
            if !source.is_empty() && seen.insert(source.as_str()) {
                if unique.len() == MAX_SOURCES {
                    self.sources_truncated = true;
                    break;
                }
                unique.push(source.clone());
            }
        }
        self.sources = unique;
    }

    pub(crate) fn set_art(&mut self, state: Option<&MediaState>, art: ArtMosaic) {
        if state.and_then(|s| s.art_url.as_deref()) == Some(art.url.as_str()) {
            self.art_requested = None;
            self.art_identity = Some(art.url.clone());
            self.art = Some(art);
        }
    }

    pub(crate) fn set_art_enabled(&mut self, enabled: bool) {
        self.art_enabled = enabled;
        if !enabled {
            self.art = None;
            self.art_identity = None;
            self.art_requested = None;
        }
    }

    pub(crate) fn art_visible(&self) -> bool {
        self.art_enabled
    }

    pub(crate) fn wants_art(
        &mut self,
        state: Option<&MediaState>,
        show_art: bool,
    ) -> Option<String> {
        if !show_art || !self.art_enabled {
            return None;
        }
        let url = state?.art_url.as_ref()?;
        if url.len() > crate::media_art::MAX_URL_LEN {
            return None;
        }
        if self.art_identity.as_deref() == Some(url.as_str())
            || self.art_requested.as_deref() == Some(url.as_str())
        {
            return None;
        }
        self.art_requested = Some(url.clone());
        Some(url.clone())
    }

    #[cfg(test)]
    pub(crate) fn needs_queue(&self, state: Option<&MediaState>) -> bool {
        state.is_some() && self.queue_request.is_some()
    }

    pub(crate) fn caps(&self) -> Option<MediaCaps> {
        self.caps
    }

    pub(crate) fn source_at(&self, index: usize) -> Option<&str> {
        self.sources.get(index).map(String::as_str)
    }

    pub(crate) fn queue_at(&self, index: usize) -> Option<&QueueItem> {
        index
            .checked_sub(self.sources.len())
            .and_then(|i| self.queue.get(i))
    }

    pub(crate) fn queue_lease_at(&self, index: usize) -> Option<QueueItemLease> {
        let item = self.queue_at(index)?;
        let (player, _) = self.queue_identity.as_ref()?;
        Some(QueueItemLease {
            id: item.id.clone(),
            player: player.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> MediaState {
        MediaState {
            player: "player".into(),
            title: "track".into(),
            art_url: Some("file:///cover.jpg".into()),
            ..Default::default()
        }
    }

    #[test]
    fn queue_request_is_single_flight_per_snapshot_identity() {
        let mut panel = MediaPanelState::default();
        let current = state();
        panel.sync_snapshot(Some(&current), Some(MediaCaps::default()), 0);
        assert!(panel.needs_queue(Some(&current)));
        panel.begin_request(Some(&current));
        let _ = panel.take_queue_request(Some(&current));
        assert!(!panel.needs_queue(Some(&current)));
        let mut changed = current.clone();
        changed.track_id = Some("next".into());
        panel.sync_snapshot(Some(&changed), Some(MediaCaps::default()), 0);
        assert!(panel.needs_queue(Some(&changed)));
    }

    #[test]
    fn artwork_request_is_single_flight_until_delivery_or_reopen() {
        let mut panel = MediaPanelState::default();
        let current = state();
        panel.set_art_enabled(true);
        assert!(panel.wants_art(Some(&current), true).is_some());
        assert!(panel.wants_art(Some(&current), true).is_none());
        panel.begin_request(Some(&current));
        assert!(panel.wants_art(Some(&current), true).is_some());
    }

    #[test]
    fn oversized_art_url_is_not_requested_or_retained() {
        let mut panel = MediaPanelState::default();
        panel.set_art_enabled(true);
        let oversized = MediaState {
            art_url: Some(format!(
                "https://example.com/{}",
                "a".repeat(crate::media_art::MAX_URL_LEN)
            )),
            ..state()
        };
        assert!(panel.wants_art(Some(&oversized), true).is_none());
        assert!(panel.art_requested.is_none());
    }

    #[test]
    fn disabling_art_clears_cached_art_and_hides_it() {
        let mut panel = MediaPanelState::default();
        assert!(!panel.art_visible());
        panel.set_art_enabled(true);
        assert!(panel.art_visible());
        panel.set_art_enabled(false);
        assert!(!panel.art_visible());
        assert!(panel.art.is_none());
    }

    #[test]
    fn source_delivery_is_deduplicated_and_is_identity_checked() {
        let mut panel = MediaPanelState::default();
        let first = state();
        panel.sync_snapshot(Some(&first), Some(MediaCaps::default()), 0);
        let request = panel.take_sources_request(Some(&first)).unwrap();
        panel.set_sources(
            request.clone(),
            Some(&first),
            vec!["one".into(), "one".into(), "two".into(), String::new()],
        );
        assert_eq!(panel.sources, ["one", "two"]);

        let mut returned = first.clone();
        returned.track_id = Some("next".into());
        panel.sync_snapshot(Some(&returned), Some(MediaCaps::default()), 0);
        let current = panel.take_sources_request(Some(&returned)).unwrap();
        panel.set_sources(request, Some(&returned), vec!["stale".into()]);
        assert_eq!(panel.sources, ["one", "two"]);
        panel.set_sources(current, Some(&returned), vec!["new".into(), "new".into()]);
        assert_eq!(panel.sources, ["new"]);
    }

    #[test]
    fn source_delivery_caps_unique_rows_in_first_seen_order() {
        let mut panel = MediaPanelState::default();
        let current = state();
        panel.sync_snapshot(Some(&current), Some(MediaCaps::default()), 0);
        let request = panel.take_sources_request(Some(&current)).unwrap();
        let sources = (0..MAX_SOURCES + 10)
            .map(|index| format!("source-{index}"))
            .collect();
        panel.set_sources(request, Some(&current), sources);

        assert_eq!(panel.sources.len(), MAX_SOURCES);
        assert!(panel.sources_truncated);
        assert_eq!(panel.sources.first().map(String::as_str), Some("source-0"));
        assert_eq!(
            panel.sources[MAX_SOURCES - 1],
            format!("source-{}", MAX_SOURCES - 1)
        );
    }

    #[test]
    fn queue_delivery_caps_rows_and_records_dropped_count() {
        let mut panel = MediaPanelState::default();
        let current = state();
        panel.begin_request(Some(&current));
        let request = panel.take_queue_request(Some(&current)).unwrap();
        let queue = (0..MAX_QUEUE_ITEMS * 2)
            .map(|index| QueueItem {
                id: index.to_string(),
                ..Default::default()
            })
            .collect();
        panel.set_queue(request, Some(&current), Ok(queue));

        assert_eq!(panel.queue.len(), MAX_QUEUE_ITEMS);
        assert_eq!(panel.queue_truncated, MAX_QUEUE_ITEMS);
    }

    #[test]
    fn queue_lease_carries_the_player_that_listed_the_queue() {
        let mut panel = MediaPanelState::default();
        let current = state();
        panel.begin_request(Some(&current));
        let request = panel.take_queue_request(Some(&current)).unwrap();
        panel.set_queue(
            request,
            Some(&current),
            Ok(vec![QueueItem {
                id: "q1".into(),
                ..Default::default()
            }]),
        );

        assert_eq!(
            panel.queue_lease_at(0),
            Some(QueueItemLease {
                id: "q1".into(),
                player: "player".into(),
            })
        );
    }

    #[test]
    fn queue_lease_is_none_after_the_player_changes() {
        let mut panel = MediaPanelState::default();
        let current = state();
        panel.begin_request(Some(&current));
        let request = panel.take_queue_request(Some(&current)).unwrap();
        panel.set_queue(
            request,
            Some(&current),
            Ok(vec![QueueItem {
                id: "q1".into(),
                ..Default::default()
            }]),
        );

        let changed = MediaState {
            player: "other".into(),
            ..current
        };
        panel.sync_snapshot(Some(&changed), Some(MediaCaps::default()), 0);

        assert_eq!(panel.queue_lease_at(0), None);
    }

    #[test]
    fn same_identity_after_generation_change_rejects_old_queue_delivery() {
        let mut panel = MediaPanelState::default();
        let current = state();
        panel.sync_snapshot(Some(&current), Some(MediaCaps::default()), 0);
        let old = panel.take_queue_request(Some(&current)).unwrap();
        panel.begin_request(Some(&current));
        let fresh = panel.take_queue_request(Some(&current)).unwrap();
        panel.set_queue(
            old,
            Some(&current),
            Ok(vec![QueueItem {
                title: "stale".into(),
                ..Default::default()
            }]),
        );
        assert!(panel.queue.is_empty());
        panel.set_queue(
            fresh,
            Some(&current),
            Ok(vec![QueueItem {
                title: "fresh".into(),
                ..Default::default()
            }]),
        );
        assert_eq!(panel.queue[0].title, "fresh");
    }

    #[test]
    fn watcher_restart_rejects_old_queue_and_source_deliveries() {
        let mut panel = MediaPanelState::default();
        let current = state();
        panel.sync_snapshot(Some(&current), Some(MediaCaps::default()), 1);
        let old_queue = panel.take_queue_request(Some(&current)).unwrap();
        let old_sources = panel.take_sources_request(Some(&current)).unwrap();

        // The displayed snapshot has not changed yet, but the selected
        // provider/config has restarted the watcher.
        panel.sync_snapshot(Some(&current), Some(MediaCaps::default()), 2);
        panel.set_queue(
            old_queue,
            Some(&current),
            Ok(vec![QueueItem {
                title: "stale".into(),
                ..Default::default()
            }]),
        );
        panel.set_sources(old_sources, Some(&current), vec!["stale".into()]);
        assert!(panel.queue.is_empty());
        assert!(panel.sources.is_empty());
    }

    #[test]
    fn queue_page_delivery_reports_backend_hidden_rows() {
        let mut panel = MediaPanelState::default();
        let current = state();
        panel.begin_request(Some(&current));
        let request = panel.take_queue_request(Some(&current)).unwrap();
        panel.set_queue_page(
            request,
            Some(&current),
            Ok(QueuePage {
                items: (0..3)
                    .map(|index| QueueItem {
                        id: format!("row-{index}"),
                        ..Default::default()
                    })
                    .collect(),
                total: 900,
            }),
        );

        assert_eq!(panel.queue.len(), 3);
        assert_eq!(panel.queue_truncated, 897);
    }
}
