//! Provider-agnostic media-player model — re-exported from the `thegn-media`
//! leaf crate so existing `thegn_core::media::*` paths keep working.
//!
//! The types moved out of core into a C-dep-free leaf so the per-OS control
//! backends (MPRIS / SMTC / mpv / AppleScript) and their model can be
//! `cargo check --target`-ed for macOS + Windows on a Linux box (the leaf can't
//! depend on core, which compiles C via rusqlite/tree-sitter). Config stays
//! here; see [`crate::config::MediaConfig::resolve_opts`] for the lowering into
//! the leaf's `ResolveOpts`.

pub use thegn_media::model::*;
pub use thegn_media::{MediaCaps, MediaSnapshot};

/// The width tiers shared by the right-panel sections. Keeping this small
/// value in the substrate-free layer lets the host map its layout enum without
/// making policy depend on termwiz or a rendered surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaWidth {
    /// Compact summary and transport controls.
    Normal,
    /// Source list plus the selected source's detail.
    Half,
    /// Source list, detail, queue, and all available decoration.
    Full,
}

/// Pure data decisions for the media panel. It deliberately contains no rows,
/// terminal cells, or provider handles: callers use these flags to project a
/// [`MediaState`] and [`MediaCaps`] into their own rendering vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MediaRenderPolicy {
    pub show_sources: bool,
    pub show_detail: bool,
    pub show_queue: bool,
    pub show_art: bool,
    pub show_transport: bool,
    pub show_shuffle: bool,
    pub show_loop: bool,
    pub show_volume: bool,
    pub show_seek: bool,
    pub show_chapters: bool,
    pub show_fullscreen: bool,
    /// Animation is only useful for active playback; paused and stopped media
    /// must not acquire a ticker merely because the panel is visible.
    pub animate: bool,
}

impl MediaRenderPolicy {
    /// Project the current state and provider capabilities into a width tier.
    /// Optional controls are visible only when both the provider advertises
    /// them and the current snapshot can support them.
    pub fn for_width(width: MediaWidth, state: &MediaState, caps: MediaCaps) -> Self {
        let detailed = matches!(width, MediaWidth::Half | MediaWidth::Full);
        let full = matches!(width, MediaWidth::Full);
        Self {
            show_sources: detailed,
            show_detail: true,
            show_queue: full && caps.queue,
            show_art: full && caps.art && state.art_url.is_some(),
            show_transport: true,
            show_shuffle: detailed && caps.shuffle,
            show_loop: detailed && caps.loop_mode,
            show_volume: detailed && caps.volume,
            show_seek: detailed && caps.seek && state.can_seek,
            show_chapters: detailed && caps.chapters,
            show_fullscreen: detailed && caps.fullscreen,
            animate: state.state == PlaybackState::Playing,
        }
    }

    /// Alias that reads naturally at call sites which already have a panel
    /// width tier and keeps the policy constructor discoverable.
    pub fn project(state: &MediaState, caps: MediaCaps, width: MediaWidth) -> Self {
        Self::for_width(width, state, caps)
    }
}

#[cfg(test)]
mod policy_tests {
    use super::*;

    fn state() -> MediaState {
        MediaState {
            state: PlaybackState::Playing,
            can_seek: true,
            art_url: Some("file:///cover.jpg".into()),
            kind: MediaKind::Video,
            ..Default::default()
        }
    }

    fn caps() -> MediaCaps {
        MediaCaps {
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
        }
    }

    #[test]
    fn width_tiers_reveal_detail_queue_and_art_progressively() {
        let s = state();
        let c = caps();
        let normal = MediaRenderPolicy::for_width(MediaWidth::Normal, &s, c);
        assert!(!normal.show_sources && !normal.show_queue && !normal.show_art);
        let half = MediaRenderPolicy::for_width(MediaWidth::Half, &s, c);
        assert!(half.show_sources && half.show_detail);
        assert!(!half.show_queue && !half.show_art);
        let full = MediaRenderPolicy::project(&s, c, MediaWidth::Full);
        assert!(full.show_sources && full.show_queue && full.show_art);
    }

    #[test]
    fn optional_controls_follow_caps_and_snapshot() {
        let mut c = caps();
        c.seek = false;
        c.volume = false;
        c.fullscreen = false;
        let mut s = state();
        s.kind = MediaKind::Audio;
        let p = MediaRenderPolicy::for_width(MediaWidth::Full, &s, c);
        assert!(!p.show_seek && !p.show_volume && !p.show_fullscreen);
        assert!(p.show_shuffle && p.show_loop && p.show_queue);
    }

    #[test]
    fn video_kind_does_not_enable_provider_optional_controls() {
        let mut c = caps();
        c.chapters = false;
        c.fullscreen = false;
        let p = MediaRenderPolicy::for_width(MediaWidth::Full, &state(), c);
        assert!(!p.show_chapters);
        assert!(!p.show_fullscreen);
    }

    #[test]
    fn paused_and_stopped_media_do_not_animate() {
        let mut s = state();
        s.state = PlaybackState::Paused;
        assert!(!MediaRenderPolicy::for_width(MediaWidth::Full, &s, caps()).animate);
        s.state = PlaybackState::Stopped;
        assert!(!MediaRenderPolicy::for_width(MediaWidth::Full, &s, caps()).animate);
    }
}
