//! The `[sandbox.vpn]` config family — attach a worktree's sandbox to its own
//! overlay/tunnel with its own identity. Kept in a sibling module (rather than
//! the god-file `config.rs`) to keep it flat; `config.rs` re-exports
//! everything here.

use serde::{Deserialize, Serialize};

use crate::config::{config_enum, config_warn};

config_enum! {
    /// `[sandbox.vpn] provider` — which overlay/tunnel a sandbox attaches to.
    /// `none` (the default) leaves the worktree's network behavior unchanged.
    /// `headscale` is `tailscale` pointed at a self-hosted control server
    /// (`[sandbox.vpn.tailscale] login_server`).
    pub enum VpnProviderKind: "vpn provider" {
        None = "none" | "off",
        Tailscale = "tailscale" | "ts",
        Headscale = "headscale" | "hs",
        Wireguard = "wireguard" | "wg" | "wg-quick",
        Openvpn = "openvpn" | "ovpn",
        Netbird = "netbird" | "nb",
        Zerotier = "zerotier" | "zt",
        Custom = "custom" | "command",
    } default = None;
}
config_enum! {
    /// How the tunnel is realized for the sandbox.
    ///  - `sidecar` (default): a companion container owns the network namespace;
    ///    the worktree OCI container joins it via `--network container:<sidecar>`,
    ///    so its only egress is the tunnel and its capabilities stay untouched
    ///    (NET_ADMIN/TUN live in the sidecar).
    ///  - `proxy`: a userspace tunnel exposes a SOCKS5/HTTP proxy; the inner
    ///    process is pointed at it via `ALL_PROXY`/`HTTPS_PROXY`. No NET_ADMIN or
    ///    /dev/net/tun needed, but only proxy-aware traffic is tunneled (not a
    ///    containment boundary). The only honest option for bwrap/systemd.
    ///  - `in_container`: run the VPN client inside the worktree container itself
    ///    (needs NET_ADMIN + /dev/net/tun; weakens `hardened`, refused if caps
    ///    are dropped).
    ///  - `netns`: join a host-prepared named network namespace (host-toolchain
    ///    backends; best-effort, needs privilege to set up).
    pub enum VpnMode: "vpn mode" {
        Sidecar = "sidecar",
        Proxy = "proxy",
        InContainer = "in_container" | "in-container",
        Netns = "netns",
    } default = Sidecar;
}
config_enum! {
    /// What to do when the tunnel can't be brought up.
    ///  - `fail` (default): refuse to launch the sandbox (don't silently fall
    ///    back to a less-isolated network).
    ///  - `warn`: launch with the tunnel down (loud warning).
    ///  - `offline`: force `network=none` so nothing leaks onto the host network.
    pub enum VpnOnError: "vpn on_error" {
        Fail = "fail",
        Warn = "warn",
        Offline = "offline",
    } default = Fail;
}
config_enum! {
    /// How DNS resolution inside the sandbox composes with the overlay.
    ///  - `tunnel` (default): the provider owns resolution (MagicDNS / pushed
    ///    resolvers); the `network_allow`/`network_block` filter is bypassed.
    ///  - `filter-front`: chain the allow/block DNS filter in front, forwarding
    ///    to the tunnel's resolver (preserves auditing).
    ///  - `filter-only`: ignore the overlay's pushed DNS, keep the filter only.
    pub enum VpnDnsMode: "vpn dns" {
        Tunnel = "tunnel",
        FilterFront = "filter-front" | "filter_front",
        FilterOnly = "filter-only" | "filter_only",
    } default = Tunnel;
}

/// `[sandbox.vpn]` — attach this worktree's sandbox to its own overlay/tunnel
/// with its own identity, leaving host networking (including any host
/// `tailscaled`) untouched. Disabled by default (`provider = "none"`). Only the
/// selected provider's sub-table is consulted.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct VpnConfig {
    pub provider: VpnProviderKind,
    pub mode: VpnMode,
    /// Override the provider's default sidecar image. Empty = provider default.
    pub sidecar_image: String,
    /// Seconds to wait for the tunnel's readiness probe before applying
    /// `on_error`.
    pub ready_timeout_secs: u64,
    pub on_error: VpnOnError,
    pub dns: VpnDnsMode,
    /// Request an ephemeral node identity where the provider supports it
    /// (Tailscale/Headscale/NetBird), so the device auto-deregisters on teardown.
    pub ephemeral: bool,
    pub tailscale: TailscaleConfig,
    pub wireguard: WireguardConfig,
    pub openvpn: OpenvpnConfig,
    pub netbird: NetbirdConfig,
    pub zerotier: ZerotierConfig,
    pub custom: CustomVpnConfig,
}

impl Default for VpnConfig {
    fn default() -> Self {
        VpnConfig {
            provider: VpnProviderKind::None,
            mode: VpnMode::Sidecar,
            sidecar_image: String::new(),
            ready_timeout_secs: 30,
            on_error: VpnOnError::Fail,
            dns: VpnDnsMode::Tunnel,
            ephemeral: true,
            tailscale: TailscaleConfig::default(),
            wireguard: WireguardConfig::default(),
            openvpn: OpenvpnConfig::default(),
            netbird: NetbirdConfig::default(),
            zerotier: ZerotierConfig::default(),
            custom: CustomVpnConfig::default(),
        }
    }
}

impl VpnConfig {
    /// Whether a tunnel is requested at all.
    pub fn is_enabled(&self) -> bool {
        self.provider != VpnProviderKind::None
    }
}

/// `[sandbox.vpn.tailscale]` — Tailscale / Headscale. `login_server` is what
/// makes it Headscale (a self-hosted control server).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct TailscaleConfig {
    /// Auth key (secrets-ref: `"env:TS_AUTHKEY"` or `"file:~/.ts/dev.key"`).
    /// Prefer an ephemeral, pre-authorized, tagged key for dev envs.
    pub auth_key: String,
    /// Custom control server, e.g. `"https://headscale.example.com"`.
    pub login_server: String,
    /// ACL tags to advertise, e.g. `["tag:dev"]`.
    pub tags: Vec<String>,
    /// Route egress through this exit node (hostname or IP). `""` = none.
    pub exit_node: String,
    /// Accept subnet routes advertised by the tailnet.
    pub accept_routes: bool,
    /// Node name in the tailnet. `""` = derive from the container name.
    pub hostname: String,
    /// Advertise these CIDRs as subnet routes from the sandbox.
    pub advertise_routes: Vec<String>,
    /// Extra `tailscale up` flags for anything not modeled here.
    pub extra_args: Vec<String>,
}

impl Default for TailscaleConfig {
    fn default() -> Self {
        TailscaleConfig {
            auth_key: "env:TS_AUTHKEY".into(),
            login_server: String::new(),
            tags: Vec::new(),
            exit_node: String::new(),
            accept_routes: false,
            hostname: String::new(),
            advertise_routes: Vec::new(),
            extra_args: Vec::new(),
        }
    }
}

/// `[sandbox.vpn.wireguard]` — a wg-quick tunnel.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct WireguardConfig {
    /// Path to a wg-quick `.conf` (mounted into the sidecar read-only). Mutually
    /// exclusive with `config`; `config` wins if both are set.
    pub config_path: String,
    /// Inline config body (secrets-ref `"file:..."` recommended to keep keys out
    /// of the thegn config file).
    pub config: String,
}

/// `[sandbox.vpn.openvpn]` — an OpenVPN tunnel.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct OpenvpnConfig {
    /// Path to a `.ovpn` (mounted into the sidecar read-only).
    pub config_path: String,
    /// `user\npass` credentials (secrets-ref `"file:~/.ovpn/creds"`).
    pub auth_user_pass: String,
    /// Extra `openvpn` flags.
    pub extra_args: Vec<String>,
}

/// `[sandbox.vpn.netbird]` — a NetBird mesh.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct NetbirdConfig {
    /// Setup key (secrets-ref).
    pub setup_key: String,
    /// Self-hosted management URL. `""` = NetBird's hosted control plane.
    pub management_url: String,
    /// Peer hostname. `""` = derive from the container name.
    pub hostname: String,
}

/// `[sandbox.vpn.zerotier]` — a ZeroTier network.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct ZerotierConfig {
    /// 16-hex network id to join.
    pub network_id: String,
    /// Self-hosted controller/moon URL. `""` = ZeroTier's hosted controller.
    pub controller_url: String,
    /// API token (secrets-ref) used to auto-authorize the joining member.
    pub api_token: String,
}

/// `[sandbox.vpn.custom]` — the open escape hatch for any tunnel not modeled
/// above (Nebula, Tinc, a corporate IPsec script, …). The `up`/`down`/
/// `ready_check` commands run via `sh -c`; the template vars `{name}`,
/// `{netns}`, and `{worktree}` are expanded before execution.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct CustomVpnConfig {
    /// Command that establishes the tunnel.
    pub up: String,
    /// Command that tears it down (best-effort, run on teardown).
    pub down: String,
    /// Command whose exit-0 means "ready" (polled until `ready_timeout_secs`).
    pub ready_check: String,
    /// Sidecar image when `mode = "sidecar"`. `""` for proxy/netns modes.
    pub image: String,
    /// Extra env passed to the `up`/`ready_check` commands / sidecar.
    pub env: std::collections::BTreeMap<String, String>,
}
