//! `[daemon]` + `[serve]` config — the control-plane sections, split out of
//! `config.rs` (the god-file ratchet) like `config_theme`.
//!
//! `[daemon]` gates the pane daemon (a `thegn daemon` process owning the
//! portable-pty panes so they survive UI exit; on by default). `[serve]`
//! shapes `thegn serve`: remote thin-client listening and the pairing policy.

use serde::{Deserialize, Serialize};
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;

use crate::config::{config_enum, config_warn};

config_enum! {
    /// Confidentiality boundary for `thegn serve`. `direct` is plaintext and
    /// therefore loopback-only unless the separately named unsafe opt-in is
    /// set. The secure modes keep Thegn's plaintext backend on loopback and
    /// delegate the public encrypted hop to the declared boundary.
    pub enum ServeTopology : "serve transport topology" {
        Direct        = "direct",
        TlsTerminated = "tls-terminated" | "tls_terminated",
        Tunnel        = "tunnel",
    } default = Direct;
}

/// Resolved, valid remote-control exposure. One value drives listener setup,
/// advertised URLs, pairing, diagnostics, and all HTTP/WS/gRPC surfaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServeExposure {
    SafeLoopback,
    TlsTerminated,
    Tunnel,
    UnsafePlaintext,
}

impl ServeExposure {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SafeLoopback => "safe-loopback",
            Self::TlsTerminated => "tls-terminated",
            Self::Tunnel => "tunnel",
            Self::UnsafePlaintext => "unsafe-plaintext",
        }
    }
}

/// Validated projection of `[serve]` plus trusted CLI overrides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServeTransportPolicy {
    pub bind: SocketAddr,
    pub exposure: ServeExposure,
    pub advertise_host: String,
    configured_advertise_port: u16,
}

impl ServeTransportPolicy {
    pub fn http_scheme(&self) -> &'static str {
        if self.exposure == ServeExposure::TlsTerminated {
            "https"
        } else {
            "http"
        }
    }

    pub fn websocket_scheme(&self) -> &'static str {
        if self.exposure == ServeExposure::TlsTerminated {
            "wss"
        } else {
            "ws"
        }
    }

    pub fn grpc_scheme(&self) -> &'static str {
        if self.exposure == ServeExposure::TlsTerminated {
            "grpcs"
        } else {
            "grpc"
        }
    }

    /// Public port, substituting the actual listener port when a direct/tunnel
    /// backend was configured with port zero. TLS termination defaults to 443.
    pub fn advertise_port(&self, actual_bind_port: u16) -> u16 {
        if self.configured_advertise_port != 0 {
            self.configured_advertise_port
        } else if self.exposure == ServeExposure::TlsTerminated {
            443
        } else if self.bind.port() != 0 {
            self.bind.port()
        } else {
            actual_bind_port
        }
    }

    pub fn advertised_origin(&self, actual_bind_port: u16) -> String {
        let host = if self.advertise_host.contains(':') && !self.advertise_host.starts_with('[') {
            format!("[{}]", self.advertise_host)
        } else {
            self.advertise_host.clone()
        };
        format!(
            "{}://{}:{}",
            self.http_scheme(),
            host,
            self.advertise_port(actual_bind_port)
        )
    }
}

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
    /// by default — the control plane carries full PTY I/O over plaintext HTTP.
    /// A non-loopback direct bind is rejected unless the dedicated unsafe opt-in
    /// is set; secure remote topologies keep this backend on loopback.
    pub bind: String,
    /// Public confidentiality topology. `direct` is plaintext and normally
    /// loopback-only. `tls-terminated` and `tunnel` require Thegn's backend to
    /// remain loopback so an arbitrary network peer cannot impersonate the
    /// declared boundary.
    pub topology: ServeTopology,
    /// Host clients actually dial. Required for a declared TLS terminator or
    /// tunnel; optional for direct mode, where the concrete bind IP is used.
    /// This is host-only (no scheme, path, credentials, query, or fragment).
    pub advertise_host: String,
    /// Port clients actually dial. Zero derives the backend port, except that
    /// TLS termination defaults to 443.
    pub advertise_port: u16,
    /// Explicit escape hatch for plaintext on a non-loopback address. This is
    /// a global user config / CLI authority only: repo overlays have no
    /// `[serve]` surface and cannot set it. Never implied by `bind`.
    pub unsafe_allow_plaintext_non_loopback: bool,
    /// Redeemed pairings wait for in-app / `thegn pair approve` approval
    /// instead of auto-approving (possession of the single-use URL is the
    /// credential by default).
    pub require_approval: bool,
    /// Unix-socket peers get implicit admin only when native peer credentials
    /// report the daemon's effective uid. The socket is also created owner-only
    /// (0600) in a 0700 run-dir; hardening failure aborts startup while this is
    /// enabled. Set false to require ordinary scoped tokens on every request,
    /// including Unix sockets (the portability escape hatch). TCP always
    /// requires tokens.
    pub local_admin: bool,
    /// Cross-origin allowlist for browser-hosted thin clients. Empty (the
    /// default) means NO cross-origin access — a browser script from another
    /// origin gets no CORS grant and its `/v1` fetch is blocked, while
    /// non-browser clients are unaffected. Each entry is an exact origin
    /// (`https://gui.example.com`); a wildcard `*` is rejected at
    /// `config validate` (a bearer-token API must never pair `*` with
    /// credentialed fetch). A browser client is otherwise an ordinary paired
    /// thin client — it presents a `tgc1_` token and answers to the same
    /// `required_scope` table; there is no cookie/session or second auth path.
    pub cors_origins: Vec<String>,
}

impl Default for ServeConfig {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:5380".into(),
            topology: ServeTopology::Direct,
            advertise_host: String::new(),
            advertise_port: 0,
            unsafe_allow_plaintext_non_loopback: false,
            require_approval: false,
            local_admin: true,
            cors_origins: Vec::new(),
        }
    }
}

impl ServeConfig {
    /// Resolve the common confidentiality policy. `unsafe_cli` is the
    /// dedicated trusted command-line opt-in; a bind override alone cannot
    /// widen plaintext exposure.
    pub fn resolve_transport(
        &self,
        bind_override: Option<&str>,
        unsafe_cli: bool,
    ) -> Result<ServeTransportPolicy, String> {
        let bind_text = bind_override.unwrap_or(&self.bind);
        let bind = bind_text.parse::<SocketAddr>().map_err(|_| {
            format!(
                "serve.bind must be a numeric IP:port so loopback policy is unambiguous (found {bind_text:?})"
            )
        })?;
        let unsafe_allowed = self.unsafe_allow_plaintext_non_loopback || unsafe_cli;
        if self.topology != ServeTopology::Direct && unsafe_allowed {
            return Err(
                "serve.unsafe_allow_plaintext_non_loopback applies only to topology = \"direct\"; remove it from a secure topology"
                    .into(),
            );
        }
        if self.topology != ServeTopology::Direct && !bind.ip().is_loopback() {
            return Err(format!(
                "serve.topology = {:?} requires a loopback backend bind; use 127.0.0.1:PORT or [::1]:PORT so only the declared terminator/tunnel reaches plaintext",
                self.topology.as_str()
            ));
        }
        if self.topology == ServeTopology::Direct && !bind.ip().is_loopback() && !unsafe_allowed {
            return Err(
                "plaintext non-loopback control exposure is refused; keep serve.bind on loopback, select topology = \"tls-terminated\" or \"tunnel\", or explicitly set unsafe_allow_plaintext_non_loopback = true"
                    .into(),
            );
        }
        if self.topology != ServeTopology::Direct && self.advertise_host.trim().is_empty() {
            return Err(format!(
                "serve.advertise_host is required for topology = {:?}; set the host clients reach through that boundary",
                self.topology.as_str()
            ));
        }
        let advertise_host = if self.advertise_host.trim().is_empty() {
            if bind.ip().is_unspecified() {
                return Err(
                    "serve.advertise_host is required when bind uses an unspecified address".into(),
                );
            }
            bind.ip().to_string()
        } else {
            validate_advertise_host(&self.advertise_host)?;
            self.advertise_host.trim().to_string()
        };
        if self.topology == ServeTopology::Direct
            && bind.ip().is_loopback()
            && !is_loopback_advertise_host(&advertise_host)
        {
            return Err(
                "topology = \"direct\" with a loopback backend may advertise only a loopback IP or localhost name; select \"tls-terminated\" or \"tunnel\" before advertising a remote host"
                    .into(),
            );
        }
        let exposure = match self.topology {
            ServeTopology::Direct if bind.ip().is_loopback() => ServeExposure::SafeLoopback,
            ServeTopology::Direct => ServeExposure::UnsafePlaintext,
            ServeTopology::TlsTerminated => ServeExposure::TlsTerminated,
            ServeTopology::Tunnel => ServeExposure::Tunnel,
        };
        Ok(ServeTransportPolicy {
            bind,
            exposure,
            advertise_host,
            configured_advertise_port: self.advertise_port,
        })
    }
}

fn is_loopback_advertise_host(host: &str) -> bool {
    let host = host.trim();
    let literal = host
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .unwrap_or(host);
    literal
        .parse::<IpAddr>()
        .is_ok_and(|address| address.is_loopback())
        || literal.eq_ignore_ascii_case("localhost")
        || literal.to_ascii_lowercase().ends_with(".localhost")
}

fn validate_advertise_host(host: &str) -> Result<(), String> {
    crate::control::validate_control_host(host)
        .map_err(|reason| format!("serve.advertise_host {reason}"))
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
        assert_eq!(s.topology, ServeTopology::Direct);
        assert!(!s.unsafe_allow_plaintext_non_loopback);
        let policy = s.resolve_transport(None, false).unwrap();
        assert_eq!(policy.exposure, ServeExposure::SafeLoopback);
        assert_eq!(policy.http_scheme(), "http");
        assert!(!s.require_approval);
        assert!(s.local_admin);
    }

    #[test]
    fn non_loopback_plaintext_requires_the_named_unsafe_opt_in() {
        let serve = ServeConfig {
            bind: "0.0.0.0:5380".into(),
            advertise_host: "control.example.test".into(),
            ..ServeConfig::default()
        };
        let error = serve.resolve_transport(None, false).unwrap_err();
        assert!(error.contains("plaintext non-loopback"));
        assert!(error.contains("unsafe_allow_plaintext_non_loopback"));

        let policy = serve.resolve_transport(None, true).unwrap();
        assert_eq!(policy.exposure, ServeExposure::UnsafePlaintext);
        assert_eq!(policy.http_scheme(), "http");
        assert_eq!(policy.websocket_scheme(), "ws");
        assert_eq!(policy.grpc_scheme(), "grpc");
    }

    #[test]
    fn bind_override_alone_cannot_widen_plaintext_exposure() {
        let mut serve = ServeConfig::default();
        assert!(
            serve
                .resolve_transport(Some("0.0.0.0:5380"), false)
                .is_err()
        );
        serve.advertise_host = "control.example.test".into();
        assert_eq!(
            serve
                .resolve_transport(Some("0.0.0.0:5380"), true)
                .unwrap()
                .exposure,
            ServeExposure::UnsafePlaintext
        );
    }

    #[test]
    fn safe_direct_cannot_advertise_an_unverified_remote_endpoint() {
        let mut serve = ServeConfig {
            advertise_host: "public.example.test".into(),
            ..ServeConfig::default()
        };
        let error = serve.resolve_transport(None, false).unwrap_err();
        assert!(error.contains("may advertise only a loopback IP or localhost"));

        for local in ["localhost", "thegn.localhost", "127.0.0.2", "[::1]"] {
            serve.advertise_host = local.into();
            assert_eq!(
                serve.resolve_transport(None, false).unwrap().exposure,
                ServeExposure::SafeLoopback
            );
        }

        serve.topology = ServeTopology::TlsTerminated;
        serve.advertise_host = "public.example.test".into();
        assert_eq!(
            serve.resolve_transport(None, false).unwrap().exposure,
            ServeExposure::TlsTerminated
        );
    }

    #[test]
    fn tls_termination_requires_loopback_boundary_and_advertises_secure_schemes() {
        let mut serve = ServeConfig {
            topology: ServeTopology::TlsTerminated,
            advertise_host: "control.example.test".into(),
            ..ServeConfig::default()
        };
        let policy = serve.resolve_transport(None, false).unwrap();
        assert_eq!(policy.exposure, ServeExposure::TlsTerminated);
        assert_eq!(policy.http_scheme(), "https");
        assert_eq!(policy.websocket_scheme(), "wss");
        assert_eq!(policy.grpc_scheme(), "grpcs");
        assert_eq!(
            policy.advertised_origin(policy.bind.port()),
            "https://control.example.test:443"
        );

        serve.bind = "10.0.0.2:5380".into();
        assert!(serve.resolve_transport(None, false).is_err());
    }

    #[test]
    fn declared_secure_topology_must_be_complete_and_non_contradictory() {
        let mut serve = ServeConfig {
            topology: ServeTopology::Tunnel,
            ..ServeConfig::default()
        };
        assert!(
            serve
                .resolve_transport(None, false)
                .unwrap_err()
                .contains("advertise_host")
        );
        serve.advertise_host = "127.0.0.1".into();
        assert_eq!(
            serve.resolve_transport(None, false).unwrap().exposure,
            ServeExposure::Tunnel
        );
        serve.unsafe_allow_plaintext_non_loopback = true;
        assert!(serve.resolve_transport(None, false).is_err());
    }

    #[test]
    fn advertised_host_is_host_only_and_ipv6_safe() {
        let mut serve = ServeConfig {
            topology: ServeTopology::TlsTerminated,
            advertise_host: "control.example.test:8443".into(),
            ..ServeConfig::default()
        };
        assert!(
            serve
                .resolve_transport(None, false)
                .unwrap_err()
                .contains("must not include a port")
        );

        serve.advertise_host = "2001:db8::1".into();
        serve.advertise_port = 8443;
        assert_eq!(
            serve
                .resolve_transport(None, false)
                .unwrap()
                .advertised_origin(5380),
            "https://[2001:db8::1]:8443"
        );

        for injected in [
            "evil&secure=1",
            "evil=1",
            "evil%26secure%3D1",
            "[::1",
            "-bad.example",
        ] {
            serve.advertise_host = injected.into();
            assert!(serve.resolve_transport(None, false).is_err(), "{injected}");
        }
    }
}
