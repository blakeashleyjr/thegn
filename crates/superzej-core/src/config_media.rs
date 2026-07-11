//! `[media]` config lowering — the `MediaConfig` inherent impls, split out of the
//! (ratcheted) `config.rs` god-file. Turns the user-facing `[media]` table into
//! the backend-resolution input the `superzej-media` leaf consumes, and picks the
//! per-kind seek step.

use serde::{Deserialize, Serialize};

use crate::config::{MediaBackendKind, MediaConfig};

/// `[media.mpd]` — native MPD backend. Talks the MPD line protocol directly, so
/// any MPD client (mpd, mpc, rmpc, ncmpcpp, cantata) is picked up with no
/// `mpd-mpris` bridge. `socket` is a `host:port` (default `127.0.0.1:6600`) or an
/// absolute path to MPD's unix socket. `$MPD_HOST`/`$MPD_PORT` override at runtime
/// when `socket` is left at its default. `password` is sent if MPD requires one.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct MpdMediaConfig {
    pub socket: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
}

impl Default for MpdMediaConfig {
    fn default() -> Self {
        MpdMediaConfig {
            socket: "127.0.0.1:6600".into(),
            password: None,
        }
    }
}

impl MediaConfig {
    /// The seek step for the given media kind: coarser for video.
    pub fn seek_step(&self, kind: superzej_media::model::MediaKind) -> std::time::Duration {
        let secs = if kind.is_video() {
            self.seek_step_video_secs
        } else {
            self.seek_step_secs
        };
        std::time::Duration::from_secs(secs)
    }

    /// Lower this config into the backend-resolution input the `superzej-media`
    /// leaf consumes (the leaf must not depend on core). When disabled the
    /// backend maps to `None`, so `superzej_media::client_for` stays inert.
    pub fn resolve_opts(&self) -> superzej_media::ResolveOpts {
        use superzej_media::BackendKind;
        let backend = if !self.enabled {
            BackendKind::None
        } else {
            match self.backend {
                MediaBackendKind::Auto => BackendKind::Auto,
                MediaBackendKind::None => BackendKind::None,
                MediaBackendKind::Mpris => BackendKind::Mpris,
                MediaBackendKind::Mpv => BackendKind::Mpv,
                MediaBackendKind::Mpd => BackendKind::Mpd,
                MediaBackendKind::Smtc => BackendKind::Smtc,
                MediaBackendKind::AppleScript => BackendKind::AppleScript,
                MediaBackendKind::Jellyfin => BackendKind::Jellyfin,
            }
        };
        superzej_media::ResolveOpts {
            backend,
            players_priority: self.players_priority.clone(),
            mpv_socket: self.mpv.socket.clone(),
            mpd_socket: self.mpd.socket.clone(),
            mpd_password: self.mpd.password.clone(),
        }
    }
}
