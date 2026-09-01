//! Loop-owned state for the docked Media section.

use thegn_core::media::{MediaCaps, MediaState, QueueItem};

use crate::media_art::ArtMosaic;

/// Source/queue selection and identity-checked asynchronous decoration. Late
/// queue or art deliveries are ignored after a player/track change.
#[derive(Debug, Default)]
pub(crate) struct MediaPanelState {
    pub(crate) sources: Vec<String>,
    pub(crate) queue: Vec<QueueItem>,
    pub(crate) art: Option<ArtMosaic>,
    queue_identity: Option<(String, Option<String>)>,
    art_identity: Option<String>,
}

impl MediaPanelState {
    fn identity(state: &MediaState) -> (String, Option<String>) {
        (
            state.player.clone(),
            state.track_id.clone().or_else(|| Some(state.now_playing())),
        )
    }

    pub(crate) fn sync_snapshot(&mut self, state: Option<&MediaState>) {
        let Some(state) = state else {
            self.sources.clear();
            self.queue.clear();
            self.art = None;
            self.queue_identity = None;
            self.art_identity = None;
            return;
        };
        if !state.player.is_empty() && !self.sources.contains(&state.player) {
            self.sources.push(state.player.clone());
        }
        let identity = Self::identity(state);
        if self.queue_identity.as_ref() != Some(&identity) {
            self.queue.clear();
            self.queue_identity = None;
        }
        if self.art_identity.as_deref() != state.art_url.as_deref() {
            self.art = None;
            self.art_identity = None;
        }
    }

    pub(crate) fn begin_request(&mut self, state: Option<&MediaState>) {
        self.queue.clear();
        self.queue_identity = state.map(Self::identity);
        if let Some(state) = state
            && self.art_identity.as_deref() != state.art_url.as_deref()
        {
            self.art = None;
            self.art_identity = None;
        }
    }

    pub(crate) fn set_queue(&mut self, state: Option<&MediaState>, queue: Vec<QueueItem>) {
        let Some(state) = state else { return };
        if self.queue_identity.as_ref() == Some(&Self::identity(state)) {
            self.queue = queue;
        }
    }

    pub(crate) fn set_art(&mut self, state: Option<&MediaState>, art: ArtMosaic) {
        if state.and_then(|s| s.art_url.as_deref()) == Some(art.url.as_str()) {
            self.art_identity = Some(art.url.clone());
            self.art = Some(art);
        }
    }

    pub(crate) fn wants_art(&self, state: Option<&MediaState>, show_art: bool) -> Option<String> {
        if !show_art {
            return None;
        }
        let url = state?.art_url.as_ref()?;
        (self.art_identity.as_deref() != Some(url.as_str())).then(|| url.clone())
    }

    pub(crate) fn source_at(&self, index: usize) -> Option<&str> {
        self.sources.get(index).map(String::as_str)
    }

    pub(crate) fn queue_at(&self, index: usize) -> Option<&QueueItem> {
        index
            .checked_sub(self.sources.len())
            .and_then(|i| self.queue.get(i))
    }

    pub(crate) fn caps(&self, state: &MediaState) -> MediaCaps {
        MediaCaps {
            shuffle: state.shuffle.is_some(),
            loop_mode: state.loop_mode.is_some(),
            volume: state.volume.is_some(),
            playlists: false,
            signals: false,
            seek: state.can_seek,
            art: state.art_url.is_some(),
            queue: !self.queue.is_empty(),
            abs_volume: state.volume.is_some(),
            chapters: state.kind.is_video(),
            fullscreen: state.kind.is_video(),
        }
    }
}
