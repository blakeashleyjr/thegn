//! `[media]` config lowering — the `MediaConfig` inherent impls, split out of the
//! (ratcheted) `config.rs` god-file. Turns the user-facing `[media]` table into
//! the backend-resolution input the `thegn-media` leaf consumes, and picks the
//! per-kind seek step.

use serde::{Deserialize, Serialize};

use crate::config::{MediaBackendKind, MediaConfig};
use crate::secretref::{BareAs, SecretRef};

/// `[media.mpd]` — native MPD backend. Talks the MPD line protocol directly, so
/// any MPD client (mpd, mpc, rmpc, ncmpcpp, cantata) is picked up with no
/// `mpd-mpris` bridge. `socket` is a `host:port` (default `127.0.0.1:6600`) or an
/// absolute path to MPD's unix socket. `$MPD_HOST`/`$MPD_PORT` override at runtime
/// when `socket` is left at its default. `password` accepts SecretRefs and keeps
/// historic bare-literal compatibility.
#[derive(Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct MpdMediaConfig {
    pub socket: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Option<String>")]
    pub password: Option<MpdSecretRef>,
}

/// A typed MPD credential reference. Legacy bare strings are represented as
/// redacted literals and remain serializable for backwards compatibility.
#[derive(Clone, PartialEq, Eq)]
pub struct MpdSecretRef(SecretRef);

impl MpdSecretRef {
    pub fn parse(value: &str) -> Self {
        let trimmed = value.trim();
        let is_reference = ["keyring:", "env:", "file:"]
            .iter()
            .any(|scheme| trimmed.starts_with(scheme));
        if is_reference {
            Self(SecretRef::parse(value, BareAs::Literal))
        } else {
            // Older MPD configs passed every byte of a bare value to MPD.
            // Preserve it so whitespace-bearing legacy credentials still work.
            Self(SecretRef::Literal(crate::secretref::LiteralSecret::new(
                value.to_string(),
            )))
        }
    }

    pub fn secret_ref(&self) -> &SecretRef {
        &self.0
    }

    pub fn is_literal(&self) -> bool {
        self.0.is_literal()
    }

    pub fn expose_literal(&self) -> Option<&str> {
        self.0.expose_literal()
    }
}

impl From<String> for MpdSecretRef {
    fn from(value: String) -> Self {
        Self::parse(&value)
    }
}

impl From<&str> for MpdSecretRef {
    fn from(value: &str) -> Self {
        Self::parse(value)
    }
}

impl std::fmt::Debug for MpdSecretRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl Serialize for MpdSecretRef {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        if let Some(reference) = self.0.to_config_string() {
            serializer.serialize_str(&reference)
        } else if let Some(literal) = self.0.expose_literal() {
            serializer.serialize_str(literal)
        } else {
            serializer.serialize_none()
        }
    }
}

impl<'de> Deserialize<'de> for MpdSecretRef {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Ok(Self::parse(&value))
    }
}

impl std::fmt::Debug for MpdMediaConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MpdMediaConfig")
            .field("socket", &self.socket)
            .field("password", &self.password.as_ref().map(|_| "[REDACTED]"))
            .finish()
    }
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
                MediaBackendKind::Mpd => BackendKind::Mpd,
                MediaBackendKind::Smtc => BackendKind::Smtc,
                MediaBackendKind::AppleScript => BackendKind::AppleScript,
                MediaBackendKind::Spotify => BackendKind::Spotify,
                MediaBackendKind::Jellyfin => BackendKind::Jellyfin,
            }
        };
        thegn_media::ResolveOpts {
            backend,
            players_priority: self.players_priority.clone(),
            mpv_socket: self.mpv.socket.clone(),
            mpd_socket: self.mpd.socket.clone(),
            mpd_password: None,
        }
    }
}
