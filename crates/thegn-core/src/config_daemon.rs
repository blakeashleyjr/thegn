//! `[daemon]` + `[serve]` config — the control-plane sections, split out of
//! `config.rs` (the god-file ratchet) like `config_theme`.
//!
//! `[daemon]` gates the pane daemon (a `thegn daemon` process owning the
//! portable-pty panes so they survive UI exit; on by default). `[serve]`
//! shapes `thegn serve`: remote thin-client listening and the pairing policy.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// `[daemon]` — the pane daemon. ON by default: new local center panes route
/// through the daemon and survive quitting the UI (bare `thegn`
/// warm-reattaches them — tmux semantics). `enabled = false` restores plain
/// in-process PTYs that die with the compositor.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct DaemonConfig {
    /// Route new panes through the pane daemon.
    pub enabled: bool,
    /// Control-socket override; empty ⇒ resolved per [`DaemonConfig::socket_path`].
    pub socket: String,
    /// Exit after this long with no live sessions; `0` = never. Ignored by
    /// `thegn serve` — a serving daemon keeps its TCP listener up for thin
    /// clients that haven't connected yet, so it never idle-exits.
    pub idle_exit_secs: u64,
    /// Keep a detached session's PTY warm this long (the relay lease grace);
    /// `0` = never reap — a detached session lives until explicitly killed
    /// (or the machine restarts).
    pub lease_grace_secs: u64,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            socket: String::new(),
            idle_exit_secs: 1800,
            lease_grace_secs: 0,
        }
    }
}

impl DaemonConfig {
    /// Resolve the control-socket path: the explicit `socket` override, else
    /// `$XDG_RUNTIME_DIR/thegn/daemon.sock`, else
    /// `<state_dir>/run/daemon.sock` (the state-dir fallback keeps
    /// `just start` / smoke isolation working — an isolated `XDG_STATE_HOME`
    /// gets an isolated daemon). Pure: env is injected.
    pub fn socket_path(&self, runtime_dir: Option<&str>, state_dir: &std::path::Path) -> PathBuf {
        if !self.socket.is_empty() {
            return PathBuf::from(&self.socket);
        }
        match runtime_dir.filter(|d| !d.is_empty()) {
            Some(run) => PathBuf::from(run).join("thegn").join("daemon.sock"),
            None => state_dir.join("run").join("daemon.sock"),
        }
    }
}

/// Longest usable unix-socket path for the platform, in bytes.
///
/// `sockaddr_un.sun_path` is 104 bytes on macOS/BSD and 108 on Linux, and the
/// path must leave room for the NUL — so the usable maximum is one less. std
/// checks this *itself* before the syscall and returns a bare `InvalidInput`
/// ("path must be shorter than SUN_LEN"), which names neither the limit nor the
/// offending path, so callers that want a usable diagnostic must pre-check.
///
/// Keyed on `linux` rather than `macos` deliberately: Linux is the outlier at
/// 108: macOS, the BSDs and illumos all use 104, so anything not-Linux takes the
/// tighter bound and an unrecognised unix errs toward caution instead of
/// promising 4 bytes it may not have.
///
/// The platform is an argument, not a `cfg!`, so both arms stay unit-testable on
/// one host — the same idiom as `thegn_svc::ipc::IpcEndpoint::classify`. Callers
/// pass `cfg!(target_os = ...)`; core carries no `libc` dependency to read
/// `sun_path` at runtime.
pub const fn max_socket_path_len(linux: bool) -> usize {
    if linux { 107 } else { 103 }
}

/// A short control-socket path under `short_dir`, standing in for a `natural`
/// path that does not fit `sun_path`.
///
/// The filename is a hash of the natural path, which is what preserves the
/// one-daemon-per-`XDG_STATE_HOME` isolation the natural path gave for free:
/// two state dirs keep two sockets. This is deliberately the same trick the
/// Windows arm already uses (`ipc::pipe_name_for_path`) — a fixed-length name
/// derived from the path — just applied to unix, where it was missing.
///
/// [`crate::util::short_hash`] rather than a digest: it exists for exactly this
/// "collision-defusing suffix" job, and 12 base36 chars consumes essentially the
/// whole 64-bit hash. The hash is an isolation key, not a security boundary —
/// the directory's 0700 ownership is what keeps other users out.
pub fn short_socket_path(short_dir: &std::path::Path, natural: &std::path::Path) -> PathBuf {
    let key = crate::util::short_hash(&natural.to_string_lossy(), 12);
    short_dir.join(format!("thegn-{key}.sock"))
}

/// Pick the control-socket path to actually use.
///
/// `natural` wins whenever it fits — so nothing moves for the overwhelming
/// majority, and no existing daemon is stranded. Only an over-long path falls
/// back to `short_dir`, and only if that genuinely fits; otherwise `natural` is
/// returned unchanged and the caller's length check degrades as before. Pure:
/// the caller resolves and vets `short_dir` from the ambient environment.
pub fn resolve_socket_path(
    natural: PathBuf,
    short_dir: Option<&std::path::Path>,
    max: usize,
    windows: bool,
) -> PathBuf {
    if check_socket_path_len(&natural, max, windows).is_ok() {
        return natural;
    }
    match short_dir.map(|d| short_socket_path(d, &natural)) {
        Some(short) if check_socket_path_len(&short, max, windows).is_ok() => short,
        _ => natural,
    }
}

/// Why a control-socket path can't be bound. Carries the numbers so the message
/// can state them rather than leaving the user to count bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SocketPathTooLong {
    pub len: usize,
    pub max: usize,
}

/// Check a resolved control-socket path against the platform limit.
///
/// Windows is exempt: its endpoints are named pipes derived by hashing this
/// path (`ipc::pipe_name_for_path`), which is length-immune by construction —
/// the very protection the unix side lacks.
pub fn check_socket_path_len(
    path: &std::path::Path,
    max: usize,
    windows: bool,
) -> Result<(), SocketPathTooLong> {
    if windows {
        return Ok(());
    }
    // Bytes, not chars: `sun_path` is a byte buffer, so a non-ASCII path costs
    // more than its character count suggests.
    let len = path.as_os_str().as_encoded_bytes().len();
    if len > max {
        return Err(SocketPathTooLong { len, max });
    }
    Ok(())
}

/// `[serve]` — remote thin-client serving + pairing policy for `thegn serve`.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct ServeConfig {
    /// Default TCP bind for `thegn serve` (overridable with `--bind`). Loopback
    /// by default — the control plane carries full PTY I/O over plaintext HTTP,
    /// so exposing it beyond localhost is opt-in. For remote thin clients, front
    /// it with a tailnet/VPN address or an explicit `--bind 0.0.0.0` behind a
    /// firewall + TLS terminator.
    pub bind: String,
    /// Redeemed pairings wait for in-app / `thegn pair approve` approval
    /// instead of auto-approving (possession of the single-use URL is the
    /// credential by default).
    pub require_approval: bool,
    /// Unix-socket peers get implicit admin so local CLI verbs need zero setup.
    /// The socket is created owner-only (0600) and its run-dir 0700, so on unix
    /// only the same uid can connect. Tokens are always required on TCP.
    pub local_admin: bool,
}

impl Default for ServeConfig {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:5380".into(),
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
    fn daemon_defaults_are_on_never_reap_and_idle_bounded() {
        let d = DaemonConfig::default();
        assert!(d.enabled, "persistence is the default (tmux semantics)");
        assert!(d.socket.is_empty());
        assert_eq!(d.idle_exit_secs, 1800, "an EMPTY daemon still exits");
        assert_eq!(
            d.lease_grace_secs, 0,
            "0 = never reap: a detached session lives until killed"
        );
    }

    #[test]
    fn socket_path_resolution_order() {
        let state = Path::new("/state/thegn");
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
            PathBuf::from("/run/user/1000/thegn/daemon.sock")
        );
        // State-dir fallback (isolated XDG_STATE_HOME ⇒ isolated daemon).
        assert_eq!(
            d.socket_path(None, state),
            PathBuf::from("/state/thegn/run/daemon.sock")
        );
        // Empty runtime dir counts as absent.
        assert_eq!(
            d.socket_path(Some(""), state),
            PathBuf::from("/state/thegn/run/daemon.sock")
        );
    }

    #[test]
    fn socket_path_limit_is_per_platform_and_boundary_exact() {
        assert_eq!(max_socket_path_len(true), 107, "Linux sun_path[108]");
        assert_eq!(
            max_socket_path_len(false),
            103,
            "macOS/BSD/illumos sun_path[104] — the cautious default"
        );

        // Exactly at the cap binds; one over does not. These boundaries are the
        // whole contract, and they were verified against a real macOS bind:
        // 103 succeeds, 104 fails with "AF_UNIX path too long".
        for linux in [true, false] {
            let max = max_socket_path_len(linux);
            let at = PathBuf::from("/".repeat(max));
            let over = PathBuf::from("/".repeat(max + 1));
            assert_eq!(check_socket_path_len(&at, max, false), Ok(()), "{linux}");
            assert_eq!(
                check_socket_path_len(&over, max, false),
                Err(SocketPathTooLong { len: max + 1, max }),
                "linux={linux}"
            );
        }

        // Windows is exempt — its pipe name is a fixed-length hash of this path.
        let huge = PathBuf::from("x".repeat(4096));
        assert_eq!(check_socket_path_len(&huge, 103, true), Ok(()));
    }

    #[test]
    fn resolved_socket_paths_are_measured_end_to_end() {
        // The realistic failure: a named profile reroots XDG_STATE_HOME under
        // ~/.thegn/profiles/<name>/state, so the path grows with both HOME and
        // the profile name. This one is 105 bytes — over the macOS cap — and
        // nothing about it is exotic.
        let d = DaemonConfig::default();
        let state = PathBuf::from(
            "/Users/blakea/.claude-profiles/regclaude/.thegn/profiles/client-acme-frontend/state/thegn",
        );
        let sock = d.socket_path(None, &state);
        let max = max_socket_path_len(false); // macOS
        assert_eq!(
            check_socket_path_len(&sock, max, false),
            Err(SocketPathTooLong { len: 105, max: 103 }),
            "path was {}",
            sock.display()
        );

        // The default (no profile) has comfortable headroom on the same HOME.
        let plain = d.socket_path(None, Path::new("/Users/blakea/.local/state/thegn"));
        assert_eq!(check_socket_path_len(&plain, max, false), Ok(()));
    }

    #[test]
    fn over_long_paths_fall_back_to_a_short_dir_and_keep_their_isolation() {
        let max = max_socket_path_len(false); // macOS
        // A real macOS per-user runtime dir: OS-created, 0700, 49 bytes.
        let short_dir = Path::new("/var/folders/3s/6g8mrdks3v36x90jfq4s8c0h0000gp/T");
        let d = DaemonConfig::default();

        // Fits ⇒ nothing moves. This is the case for almost everyone, and it
        // is what keeps existing daemons reachable.
        let plain = d.socket_path(None, Path::new("/Users/blakea/.local/state/thegn"));
        assert_eq!(
            resolve_socket_path(plain.clone(), Some(short_dir), max, false),
            plain
        );

        // Over the limit ⇒ relocated, and the result actually fits.
        let long_state = Path::new(
            "/Users/blakea/.claude-profiles/regclaude/.thegn/profiles/client-acme-frontend/state/thegn",
        );
        let long = d.socket_path(None, long_state);
        let moved = resolve_socket_path(long.clone(), Some(short_dir), max, false);
        assert_ne!(moved, long, "an unbindable path must be replaced");
        assert!(moved.starts_with(short_dir));
        assert_eq!(check_socket_path_len(&moved, max, false), Ok(()));

        // Isolation survives relocation: a different state dir ⇒ a different
        // socket, or two profiles would silently share one daemon.
        let other_state = Path::new(
            "/Users/blakea/.claude-profiles/regclaude/.thegn/profiles/client-acme-frontends/state/thegn",
        );
        let other = resolve_socket_path(
            d.socket_path(None, other_state),
            Some(short_dir),
            max,
            false,
        );
        assert_ne!(moved, other, "distinct state dirs must not collide");

        // No usable short dir ⇒ unchanged, and the caller degrades as before.
        assert_eq!(resolve_socket_path(long.clone(), None, max, false), long);

        // A short dir that is itself too deep is refused rather than trusted.
        let deep = PathBuf::from(format!("/{}", "d".repeat(120)));
        assert_eq!(
            resolve_socket_path(long.clone(), Some(&deep), max, false),
            long
        );
    }

    #[test]
    fn serve_defaults() {
        let s = ServeConfig::default();
        assert_eq!(s.bind, "127.0.0.1:5380");
        assert!(!s.require_approval);
        assert!(s.local_admin);
    }
}
