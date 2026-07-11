//! `[media]` config lowering — the `MediaConfig` inherent impls, split out of the
//! (ratcheted) `config.rs` god-file. Turns the user-facing `[media]` table into
//! the backend-resolution input the `thegn-media` leaf consumes, and picks the
//! per-kind seek step.

use crate::config::{MediaBackendKind, MediaConfig};

impl MediaConfig {
    /// The seek step for the given media kind: coarser for video.
    pub fn seek_step(&self, kind: thegn_media::model::MediaKind) -> std::time::Duration {
        let secs = if kind.is_video() {
            self.seek_step_video_secs
        } else {
            self.seek_step_secs
        };
        std::time::Duration::from_secs(secs)
    }

    /// Lower this config into the backend-resolution input the `thegn-media`
    /// leaf consumes (the leaf must not depend on core). When disabled the
    /// backend maps to `None`, so `thegn_media::client_for` stays inert.
    pub fn resolve_opts(&self) -> thegn_media::ResolveOpts {
        use thegn_media::BackendKind;
        let backend = if !self.enabled {
            BackendKind::None
        } else {
            match self.backend {
                MediaBackendKind::Auto => BackendKind::Auto,
                MediaBackendKind::None => BackendKind::None,
                MediaBackendKind::Mpris => BackendKind::Mpris,
                MediaBackendKind::Mpv => BackendKind::Mpv,
                MediaBackendKind::Smtc => BackendKind::Smtc,
                MediaBackendKind::AppleScript => BackendKind::AppleScript,
                MediaBackendKind::Jellyfin => BackendKind::Jellyfin,
            }
        };
        thegn_media::ResolveOpts {
            backend,
            players_priority: self.players_priority.clone(),
            mpv_socket: self.mpv.socket.clone(),
        }
    }
}
