//! `[daemon]` + `[serve]` config — the control-plane sections, split out of
//! `config.rs` (the god-file ratchet) like `config_theme`.
//!
//! `[daemon]` gates the pane daemon (a `szhost daemon` process owning the
//! portable-pty panes so they survive UI exit; opt-in). `[serve]` shapes
//! `szhost serve`: remote thin-client listening and the pairing policy.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// `[daemon]` — the pane daemon. Off by default: bare `szhost` keeps today's
/// in-process PTYs; when enabled (or under `szhost serve`, which implies it),
/// new panes route through the daemon and survive a client detach.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct DaemonConfig {
    /// Route new panes through the pane daemon.
    pub enabled: bool,
    /// Control-socket override; empty ⇒ resolved per [`DaemonConfig::socket_path`].
    pub socket: String,
    /// Exit after this long with no sessions and no clients; `0` = never.
    pub idle_exit_secs: u64,
    /// Keep a detached session's PTY warm this long (the relay lease grace).
    pub lease_grace_secs: u64,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            socket: String::new(),
            idle_exit_secs: 1800,
            lease_grace_secs: 3600,
        }
    }
}

impl DaemonConfig {
    /// Resolve the control-socket path: the explicit `socket` override, else
    /// `$XDG_RUNTIME_DIR/superzej/daemon.sock`, else
    /// `<state_dir>/run/daemon.sock` (the state-dir fallback keeps
    /// `just start` / smoke isolation working — an isolated `XDG_STATE_HOME`
    /// gets an isolated daemon). Pure: env is injected.
    pub fn socket_path(&self, runtime_dir: Option<&str>, state_dir: &std::path::Path) -> PathBuf {
        if !self.socket.is_empty() {
            return PathBuf::from(&self.socket);
        }
        match runtime_dir.filter(|d| !d.is_empty()) {
            Some(run) => PathBuf::from(run).join("superzej").join("daemon.sock"),
            None => state_dir.join("run").join("daemon.sock"),
        }
    }
}

/// `[serve]` — remote thin-client serving + pairing policy for `szhost serve`.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct ServeConfig {
    /// Default TCP bind for `szhost serve` (overridable with `--bind`).
    pub bind: String,
    /// Redeemed pairings wait for in-app / `szhost pair approve` approval
    /// instead of auto-approving (possession of the single-use URL is the
    /// credential by default).
    pub require_approval: bool,
    /// Unix-socket peers (same uid, via peer credentials) get implicit admin —
    /// local CLI verbs need zero setup. Tokens are always required on TCP.
    pub local_admin: bool,
}

impl Default for ServeConfig {
    fn default() -> Self {
        Self {
            bind: "0.0.0.0:5380".into(),
            require_approval: false,
            local_admin: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn daemon_defaults_are_opt_in_and_bounded() {
        let d = DaemonConfig::default();
        assert!(!d.enabled, "the daemon must be opt-in");
        assert!(d.socket.is_empty());
        assert_eq!(d.idle_exit_secs, 1800);
        assert_eq!(d.lease_grace_secs, 3600);
    }

    #[test]
    fn socket_path_resolution_order() {
        let state = Path::new("/state/superzej");
        // Explicit override wins over everything.
        let d = DaemonConfig {
            socket: "/tmp/custom.sock".into(),
            ..Default::default()
        };
        assert_eq!(
            d.socket_path(Some("/run/user/1000"), state),
            PathBuf::from("/tmp/custom.sock")
        );
        // XDG_RUNTIME_DIR next.
        let d = DaemonConfig::default();
        assert_eq!(
            d.socket_path(Some("/run/user/1000"), state),
            PathBuf::from("/run/user/1000/superzej/daemon.sock")
        );
        // State-dir fallback (isolated XDG_STATE_HOME ⇒ isolated daemon).
        assert_eq!(
            d.socket_path(None, state),
            PathBuf::from("/state/superzej/run/daemon.sock")
        );
        // Empty runtime dir counts as absent.
        assert_eq!(
            d.socket_path(Some(""), state),
            PathBuf::from("/state/superzej/run/daemon.sock")
        );
    }

    #[test]
    fn serve_defaults() {
        let s = ServeConfig::default();
        assert_eq!(s.bind, "0.0.0.0:5380");
        assert!(!s.require_approval);
        assert!(s.local_admin);
    }
}
