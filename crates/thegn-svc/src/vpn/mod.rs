//! Per-sandbox VPN/tunnel attachment.
//!
//! thegn attaches a worktree's sandbox to its OWN overlay/tunnel (Tailscale,
//! Headscale, WireGuard, OpenVPN, NetBird, ZeroTier, or a custom tunnel) with
//! its own identity, leaving host networking untouched. It does NOT embed a
//! tunnel datapath: each tunnel daemon runs as a per-worktree **sidecar**
//! container (or, for the host-toolchain backends, a userspace proxy), exactly
//! as the sandbox already shells out to `podman` rather than embedding a
//! container runtime.
//!
//! This module owns provider *knowledge*: given a resolved [`VpnSpec`], build
//! the (pure, testable) [`SidecarPlan`] — image, flags, env, mounts, daemon
//! command, readiness probe — resolve its secrets, and run/probe/tear-down the
//! sidecar. The *wiring* (joining the worktree container to the sidecar's netns
//! via `--network container:<sidecar>`, suppressing `--dns`/`-p`, teardown
//! ordering) lives in `thegn_core::sandbox`, and the host sequences the two.
//!
//! Division of labor mirrors the other svc seams: the pure plan builders are
//! unit-tested here; the subprocess execution is the I/O seam (exercised by
//! `test/smoke.sh`).

use anyhow::{Context, Result, anyhow, bail};
use std::time::{Duration, Instant};
use thegn_core::config::expand_env_ref;
use thegn_core::config::{VpnDnsMode, VpnMode, VpnProviderKind};
use thegn_core::sandbox::{VpnParams, VpnSpec};

/// How to invoke the container CLI for the sidecar — the same runtime the
/// worktree container uses, so the two share a user namespace and `--network
/// container:` works. The prefix is an argv (e.g. `["podman"]` or
/// `["sudo", "-n", "podman"]`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OciRuntime {
    pub prefix: Vec<String>,
}

impl OciRuntime {
    pub fn new(prefix: Vec<String>) -> Self {
        OciRuntime { prefix }
    }
    pub fn podman() -> Self {
        OciRuntime::new(vec!["podman".into()])
    }
    pub fn docker() -> Self {
        OciRuntime::new(vec!["docker".into()])
    }
    /// Full argv for a subcommand: `<prefix> <args...>`.
    fn argv(&self, args: &[&str]) -> Vec<String> {
        let mut v = self.prefix.clone();
        v.extend(args.iter().map(|s| s.to_string()));
        v
    }
}

/// What the provider produced — consumed by `thegn_core::sandbox` to splice
/// the tunnel into the worktree sandbox. Fields compose: a userspace sidecar
/// yields both a `join_netns` and a `proxy`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Attachment {
    /// Worktree OCI container should be created with
    /// `--network container:<this>`.
    pub join_netns: Option<String>,
    /// Worktree process should be pointed at this proxy via `ALL_PROXY` etc.
    pub proxy: Option<Proxy>,
    /// bwrap/systemd should join this host-prepared netns path.
    pub host_netns: Option<String>,
}

/// A SOCKS5/HTTP proxy endpoint a userspace tunnel exposes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proxy {
    pub all_proxy: String,
    pub no_proxy: Vec<String>,
}

impl Proxy {
    /// `export` lines for `wrap_script` injection. Sorted for determinism.
    pub fn env_exports(&self) -> Vec<(String, String)> {
        let no = self.no_proxy.join(",");
        vec![
            ("ALL_PROXY".into(), self.all_proxy.clone()),
            ("all_proxy".into(), self.all_proxy.clone()),
            ("HTTP_PROXY".into(), self.all_proxy.clone()),
            ("HTTPS_PROXY".into(), self.all_proxy.clone()),
            ("NO_PROXY".into(), no.clone()),
            ("no_proxy".into(), no),
        ]
    }
}

/// DNS the sandbox should use inside the tunnel.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DnsConfig {
    /// Resolver to inject (e.g. Tailscale MagicDNS `100.100.100.100`). `None`
    /// means "the overlay's pushed resolver / the sidecar's resolv.conf governs".
    pub nameserver: Option<String>,
    pub search: Vec<String>,
}

/// Capability/device requirements a tunnel imposes — surfaced so the sandbox can
/// reconcile them with the hardening profile. In `sidecar` mode the NET_ADMIN/
/// TUN burden lands on the **sidecar**, leaving the worktree's caps untouched.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Requirements {
    pub worktree_needs_net_admin: bool,
    pub worktree_needs_tun: bool,
    pub sidecar_needs_net_admin: bool,
    pub sidecar_needs_tun: bool,
}

/// How to decide a readiness probe succeeded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadyWhen {
    /// The probe command exits 0.
    ExitZero,
    /// The probe's stdout contains this substring.
    StdoutContains(String),
}

/// A readiness probe `exec`'d inside the sidecar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadyProbe {
    pub argv: Vec<String>,
    pub when: ReadyWhen,
}

/// A file the provider needs materialized on the host (0600) and bind-mounted
/// into the sidecar — e.g. a wg `.conf` resolved from an inline secrets-ref.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SidecarFile {
    pub contents: String,
    /// Destination path inside the sidecar.
    pub dest: String,
}

/// A pure, fully-resolved plan for the sidecar container. Built from a
/// `VpnSpec` (with secrets already dereferenced); executed by `run_sidecar`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SidecarPlan {
    pub container: String,
    pub image: String,
    /// `run -d` flags emitted between `--name <container>` and the image
    /// (caps, devices, sysctls, hostname).
    pub run_flags: Vec<String>,
    /// `-e KEY=VAL` environment (secrets already resolved).
    pub env: Vec<(String, String)>,
    /// Read-only bind mounts: `(host_path, dest_path)`.
    pub mounts: Vec<(String, String)>,
    /// Files to materialize on the host before run and bind-mount in.
    pub files: Vec<SidecarFile>,
    /// Command/args after the image (the daemon). Empty = image default entrypoint.
    pub command: Vec<String>,
    pub ready: ReadyProbe,
    /// Whether the worktree must use a proxy (userspace mode) and at what address.
    pub proxy: Option<Proxy>,
}

/// The VPN provider seam. One built-in impl ([`BuiltinProvider`]) dispatches on
/// the resolved provider; external providers could plug in here later.
///
/// Methods are synchronous (blocking subprocess work, like
/// `thegn_core::sandbox::ensure`); the host runs them off the event loop on a
/// blocking thread, the same place sandbox bring-up already lives.
pub trait VpnProvider: Send + Sync {
    /// Stable id for logging/bookkeeping.
    fn kind(&self) -> &'static str;
    /// Capability/device requirements (available before bring-up).
    fn requirements(&self) -> Requirements;
    /// DNS the sandbox should use.
    fn dns(&self) -> DnsConfig;
    /// Build the (secrets-resolved) sidecar plan. Errors if a required secret
    /// (auth key, config) can't be resolved. Reads env/files.
    fn sidecar_plan(&self, container: &str) -> Result<SidecarPlan>;
    /// Bring the tunnel up and return the attachment the sandbox splices in.
    fn up(&self, rt: &OciRuntime, container: &str) -> Result<Attachment>;
    /// Block until ready or `timeout` elapses.
    fn ready(&self, rt: &OciRuntime, container: &str, timeout: Duration) -> Result<()>;
    /// Tear the tunnel down (de-register ephemeral node, then the sidecar is
    /// `rm -f`'d by `thegn_core::sandbox::teardown`). Best-effort.
    fn down(&self, rt: &OciRuntime, container: &str) -> Result<()>;
}

/// Pick the provider implementation for a resolved [`VpnSpec`]. Returns the
/// concrete [`BuiltinProvider`] (the `async fn` methods make the trait not
/// dyn-compatible without an `async_trait` shim; a concrete return keeps the
/// seam zero-cost while [`VpnProvider`] documents the contract).
pub fn for_provider(spec: &VpnSpec) -> BuiltinProvider<'_> {
    BuiltinProvider { spec }
}

/// The single built-in provider; dispatches on `spec.params`.
pub struct BuiltinProvider<'a> {
    spec: &'a VpnSpec,
}

impl VpnProvider for BuiltinProvider<'_> {
    fn kind(&self) -> &'static str {
        match self.spec.provider {
            VpnProviderKind::None => "none",
            VpnProviderKind::Tailscale => "tailscale",
            VpnProviderKind::Headscale => "headscale",
            VpnProviderKind::Wireguard => "wireguard",
            VpnProviderKind::Openvpn => "openvpn",
            VpnProviderKind::Netbird => "netbird",
            VpnProviderKind::Zerotier => "zerotier",
            VpnProviderKind::Custom => "custom",
        }
    }

    fn requirements(&self) -> Requirements {
        requirements_for(self.spec)
    }

    fn dns(&self) -> DnsConfig {
        dns_for(self.spec)
    }

    fn sidecar_plan(&self, container: &str) -> Result<SidecarPlan> {
        plan_for(self.spec, container)
    }

    fn up(&self, rt: &OciRuntime, container: &str) -> Result<Attachment> {
        let plan = self.sidecar_plan(container)?;
        run_sidecar(rt, &plan)?;
        Ok(Attachment {
            join_netns: matches!(self.spec.mode, VpnMode::Sidecar | VpnMode::Proxy)
                .then(|| container.to_string()),
            proxy: plan.proxy.clone(),
            host_netns: None,
        })
    }

    fn ready(&self, rt: &OciRuntime, container: &str, timeout: Duration) -> Result<()> {
        let plan = self.sidecar_plan(container)?;
        poll_ready(rt, container, &plan.ready, timeout)
    }

    fn down(&self, rt: &OciRuntime, container: &str) -> Result<()> {
        // De-register the ephemeral node before the container is removed.
        if let Some(argv) = deregister_argv(self.spec) {
            let _ = exec_in(rt, container, &argv);
        }
        Ok(())
    }
}

// ── pure builders (unit-tested) ──────────────────────────────────────────────

/// Default sidecar image per provider (overridable via `[sandbox.vpn]
/// sidecar_image` or the custom sub-table's `image`).
fn default_image(spec: &VpnSpec) -> Option<String> {
    if let Some(img) = &spec.sidecar_image {
        return Some(img.clone());
    }
    let img = match &spec.params {
        VpnParams::Tailscale(_) => "docker.io/tailscale/tailscale:stable",
        VpnParams::Wireguard(_) => "docker.io/linuxserver/wireguard:latest",
        VpnParams::Openvpn(_) => "docker.io/dperson/openvpn-client:latest",
        VpnParams::Netbird(_) => "docker.io/netbirdio/netbird:latest",
        VpnParams::Zerotier(_) => "docker.io/zyclonite/zerotier:latest",
        VpnParams::Custom(c) => {
            return (!c.image.trim().is_empty()).then(|| c.image.trim().to_string());
        }
    };
    Some(img.to_string())
}

/// Whether this provider/mode runs the tunnel transparently (TUN) vs userspace
/// (a SOCKS5/HTTP proxy the worktree opts into). Only Tailscale/NetBird have a
/// first-class userspace mode; everything else is TUN.
fn is_userspace(spec: &VpnSpec) -> bool {
    matches!(spec.mode, VpnMode::Proxy)
        && matches!(
            &spec.params,
            VpnParams::Tailscale(_) | VpnParams::Netbird(_)
        )
}

fn requirements_for(spec: &VpnSpec) -> Requirements {
    // TUN/NET_ADMIN burden, before deciding which container carries it.
    let needs_tun = !is_userspace(spec)
        && !matches!(&spec.params, VpnParams::Zerotier(_) | VpnParams::Custom(_));
    let needs_net_admin = needs_tun || matches!(&spec.params, VpnParams::Zerotier(_));
    match spec.mode {
        // Sidecar/proxy: the burden is on the sidecar; the worktree stays clean.
        VpnMode::Sidecar | VpnMode::Proxy => Requirements {
            worktree_needs_net_admin: false,
            worktree_needs_tun: false,
            sidecar_needs_net_admin: needs_net_admin,
            sidecar_needs_tun: needs_tun,
        },
        // In-container/netns: the worktree itself carries the burden.
        VpnMode::InContainer | VpnMode::Netns => Requirements {
            worktree_needs_net_admin: needs_net_admin,
            worktree_needs_tun: needs_tun,
            sidecar_needs_net_admin: false,
            sidecar_needs_tun: false,
        },
    }
}

fn dns_for(spec: &VpnSpec) -> DnsConfig {
    // Only meaningful for `tunnel`/`filter-front` DNS modes; `filter-only`
    // suppresses the overlay's pushed DNS.
    if spec.dns_mode == VpnDnsMode::FilterOnly {
        return DnsConfig::default();
    }
    match &spec.params {
        // Tailscale/Headscale MagicDNS.
        VpnParams::Tailscale(_) => DnsConfig {
            nameserver: Some("100.100.100.100".into()),
            search: Vec::new(),
        },
        // Others push DNS via their own config; the sidecar resolv.conf governs.
        _ => DnsConfig::default(),
    }
}

/// Resolve a secrets-ref, erroring with context when a *required* one is missing.
fn require_secret(value: &str, what: &str) -> Result<String> {
    expand_env_ref(value).ok_or_else(|| {
        anyhow!("vpn: could not resolve {what} (set the env var or file referenced by '{value}')")
    })
}

fn plan_for(spec: &VpnSpec, container: &str) -> Result<SidecarPlan> {
    let image = default_image(spec)
        .ok_or_else(|| anyhow!("vpn: no sidecar image for provider '{}'", spec_kind(spec)))?;
    match &spec.params {
        VpnParams::Tailscale(t) => plan_tailscale(spec, t, container, image),
        VpnParams::Wireguard(w) => plan_wireguard(w, container, image),
        VpnParams::Openvpn(o) => plan_openvpn(o, container, image),
        VpnParams::Netbird(n) => plan_netbird(spec, n, container, image),
        VpnParams::Zerotier(z) => plan_zerotier(z, container, image),
        VpnParams::Custom(c) => plan_custom(spec, c, container, image),
    }
}

fn spec_kind(spec: &VpnSpec) -> &'static str {
    for_provider(spec).kind()
}

fn proxy_for_port(port: u16) -> Proxy {
    Proxy {
        all_proxy: format!("socks5://127.0.0.1:{port}"),
        no_proxy: vec!["localhost".into(), "127.0.0.1".into()],
    }
}

const TS_SOCKS_PORT: u16 = 1055;

fn plan_tailscale(
    spec: &VpnSpec,
    t: &thegn_core::config::TailscaleConfig,
    container: &str,
    image: String,
) -> Result<SidecarPlan> {
    let auth = require_secret(&t.auth_key, "tailscale auth_key")?;
    let userspace = is_userspace(spec);

    let mut run_flags = vec!["--hostname".into(), spec.hostname.clone()];
    if !userspace {
        run_flags.extend([
            "--cap-add".into(),
            "NET_ADMIN".into(),
            "--device".into(),
            "/dev/net/tun".into(),
        ]);
    }

    // Build the `tailscale up` extra args carried via TS_EXTRA_ARGS.
    let mut extra: Vec<String> = vec![format!("--hostname={}", spec.hostname)];
    if !t.login_server.trim().is_empty() {
        extra.push(format!("--login-server={}", t.login_server.trim()));
    }
    if !t.tags.is_empty() {
        extra.push(format!("--advertise-tags={}", t.tags.join(",")));
    }
    if !t.exit_node.trim().is_empty() {
        extra.push(format!("--exit-node={}", t.exit_node.trim()));
    }
    if t.accept_routes {
        extra.push("--accept-routes".into());
    }
    if !t.advertise_routes.is_empty() {
        extra.push(format!(
            "--advertise-routes={}",
            t.advertise_routes.join(",")
        ));
    }
    if spec.ephemeral {
        extra.push("--ephemeral".into());
    }
    extra.extend(t.extra_args.iter().cloned());

    let mut env = vec![
        ("TS_AUTHKEY".into(), auth),
        ("TS_EXTRA_ARGS".into(), extra.join(" ")),
        (
            "TS_ACCEPT_DNS".into(),
            (spec.dns_mode != VpnDnsMode::FilterOnly).to_string(),
        ),
    ];
    if userspace {
        env.push(("TS_USERSPACE".into(), "true".into()));
        env.push((
            "TS_SOCKS5_SERVER".into(),
            format!("0.0.0.0:{TS_SOCKS_PORT}"),
        ));
    } else {
        env.push(("TS_USERSPACE".into(), "false".into()));
    }

    Ok(SidecarPlan {
        container: container.into(),
        image,
        run_flags,
        env,
        mounts: Vec::new(),
        files: Vec::new(),
        command: Vec::new(),
        ready: ReadyProbe {
            argv: vec!["tailscale".into(), "status".into(), "--json".into()],
            when: ReadyWhen::StdoutContains("\"Running\"".into()),
        },
        proxy: userspace.then(|| proxy_for_port(TS_SOCKS_PORT)),
    })
}

fn plan_wireguard(
    w: &thegn_core::config::WireguardConfig,
    container: &str,
    image: String,
) -> Result<SidecarPlan> {
    let dest = "/etc/wireguard/wg0.conf".to_string();
    let (mounts, files) = if !w.config.trim().is_empty() {
        let body = require_secret(&w.config, "wireguard config")?;
        (
            Vec::new(),
            vec![SidecarFile {
                contents: body,
                dest: dest.clone(),
            }],
        )
    } else if !w.config_path.trim().is_empty() {
        (
            vec![(w.config_path.trim().to_string(), dest.clone())],
            Vec::new(),
        )
    } else {
        bail!("vpn: wireguard requires either config_path or config");
    };
    Ok(SidecarPlan {
        container: container.into(),
        image,
        run_flags: vec![
            "--cap-add".into(),
            "NET_ADMIN".into(),
            "--device".into(),
            "/dev/net/tun".into(),
            "--sysctl".into(),
            "net.ipv4.conf.all.src_valid_mark=1".into(),
        ],
        env: Vec::new(),
        mounts,
        files,
        command: Vec::new(),
        ready: ReadyProbe {
            argv: vec!["wg".into(), "show".into()],
            when: ReadyWhen::StdoutContains("interface".into()),
        },
        proxy: None,
    })
}

fn plan_openvpn(
    o: &thegn_core::config::OpenvpnConfig,
    container: &str,
    image: String,
) -> Result<SidecarPlan> {
    if o.config_path.trim().is_empty() {
        bail!("vpn: openvpn requires config_path");
    }
    let dest = "/vpn/config.ovpn".to_string();
    let mut mounts = vec![(o.config_path.trim().to_string(), dest.clone())];
    let mut command = vec!["openvpn".into(), "--config".into(), dest];
    if !o.auth_user_pass.trim().is_empty() {
        let creds = require_secret(&o.auth_user_pass, "openvpn auth_user_pass")?;
        let cred_dest = "/vpn/creds".to_string();
        command.extend(["--auth-user-pass".into(), cred_dest.clone()]);
        // creds materialized as a file (it may be a literal "user\npass").
        return Ok(SidecarPlan {
            container: container.into(),
            image,
            run_flags: vec![
                "--cap-add".into(),
                "NET_ADMIN".into(),
                "--device".into(),
                "/dev/net/tun".into(),
            ],
            env: Vec::new(),
            mounts,
            files: vec![SidecarFile {
                contents: creds,
                dest: cred_dest,
            }],
            command: {
                command.extend(o.extra_args.iter().cloned());
                command
            },
            ready: ReadyProbe {
                argv: vec![
                    "sh".into(),
                    "-c".into(),
                    "ip link show tun0 >/dev/null 2>&1".into(),
                ],
                when: ReadyWhen::ExitZero,
            },
            proxy: None,
        });
    }
    command.extend(o.extra_args.iter().cloned());
    mounts.shrink_to_fit();
    Ok(SidecarPlan {
        container: container.into(),
        image,
        run_flags: vec![
            "--cap-add".into(),
            "NET_ADMIN".into(),
            "--device".into(),
            "/dev/net/tun".into(),
        ],
        env: Vec::new(),
        mounts,
        files: Vec::new(),
        command,
        ready: ReadyProbe {
            argv: vec![
                "sh".into(),
                "-c".into(),
                "ip link show tun0 >/dev/null 2>&1".into(),
            ],
            when: ReadyWhen::ExitZero,
        },
        proxy: None,
    })
}

fn plan_netbird(
    spec: &VpnSpec,
    n: &thegn_core::config::NetbirdConfig,
    container: &str,
    image: String,
) -> Result<SidecarPlan> {
    let key = require_secret(&n.setup_key, "netbird setup_key")?;
    let userspace = is_userspace(spec);
    let mut run_flags = vec!["--hostname".into(), spec.hostname.clone()];
    if !userspace {
        run_flags.extend([
            "--cap-add".into(),
            "NET_ADMIN".into(),
            "--device".into(),
            "/dev/net/tun".into(),
        ]);
    }
    let mut env = vec![
        ("NB_SETUP_KEY".into(), key),
        ("NB_HOSTNAME".into(), spec.hostname.clone()),
    ];
    if !n.management_url.trim().is_empty() {
        env.push(("NB_MANAGEMENT_URL".into(), n.management_url.trim().into()));
    }
    if userspace {
        env.push(("NB_USE_NETSTACK_MODE".into(), "true".into()));
    }
    Ok(SidecarPlan {
        container: container.into(),
        image,
        run_flags,
        env,
        mounts: Vec::new(),
        files: Vec::new(),
        command: Vec::new(),
        ready: ReadyProbe {
            argv: vec!["netbird".into(), "status".into()],
            when: ReadyWhen::StdoutContains("Connected".into()),
        },
        proxy: userspace.then(|| proxy_for_port(TS_SOCKS_PORT)),
    })
}

fn plan_zerotier(
    z: &thegn_core::config::ZerotierConfig,
    container: &str,
    image: String,
) -> Result<SidecarPlan> {
    if z.network_id.trim().is_empty() {
        bail!("vpn: zerotier requires network_id");
    }
    let net = z.network_id.trim().to_string();
    Ok(SidecarPlan {
        container: container.into(),
        image,
        run_flags: vec![
            "--cap-add".into(),
            "NET_ADMIN".into(),
            "--device".into(),
            "/dev/net/tun".into(),
        ],
        env: vec![("ZEROTIER_JOIN_NETWORKS".into(), net.clone())],
        mounts: Vec::new(),
        files: Vec::new(),
        command: Vec::new(),
        ready: ReadyProbe {
            argv: vec!["zerotier-cli".into(), "info".into()],
            when: ReadyWhen::StdoutContains("ONLINE".into()),
        },
        proxy: None,
    })
}

fn plan_custom(
    spec: &VpnSpec,
    c: &thegn_core::config::CustomVpnConfig,
    container: &str,
    image: String,
) -> Result<SidecarPlan> {
    if c.up.trim().is_empty() {
        bail!("vpn: custom provider requires an `up` command");
    }
    let up = expand_templates(&c.up, container, &spec.hostname);
    let mut env: Vec<(String, String)> =
        c.env.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    env.sort();
    let ready = if c.ready_check.trim().is_empty() {
        ReadyProbe {
            argv: vec!["true".into()],
            when: ReadyWhen::ExitZero,
        }
    } else {
        ReadyProbe {
            argv: vec![
                "sh".into(),
                "-c".into(),
                expand_templates(&c.ready_check, container, &spec.hostname),
            ],
            when: ReadyWhen::ExitZero,
        }
    };
    Ok(SidecarPlan {
        container: container.into(),
        image,
        run_flags: vec![
            "--cap-add".into(),
            "NET_ADMIN".into(),
            "--device".into(),
            "/dev/net/tun".into(),
        ],
        env,
        mounts: Vec::new(),
        files: Vec::new(),
        command: vec!["sh".into(), "-c".into(), up],
        ready,
        proxy: None,
    })
}

/// Expand the `{name}`/`{netns}`/`{worktree}` template vars in a custom command.
fn expand_templates(s: &str, container: &str, hostname: &str) -> String {
    s.replace("{name}", container)
        .replace("{netns}", container)
        .replace("{worktree}", hostname)
}

/// The de-registration command run inside the sidecar on teardown (ephemeral
/// nodes), or `None` when the provider has nothing to do.
fn deregister_argv(spec: &VpnSpec) -> Option<Vec<String>> {
    if !spec.ephemeral {
        return None;
    }
    match &spec.params {
        VpnParams::Tailscale(_) => Some(vec!["tailscale".into(), "logout".into()]),
        VpnParams::Netbird(_) => Some(vec!["netbird".into(), "down".into()]),
        VpnParams::Zerotier(z) if !z.network_id.trim().is_empty() => Some(vec![
            "zerotier-cli".into(),
            "leave".into(),
            z.network_id.trim().into(),
        ]),
        VpnParams::Custom(c) if !c.down.trim().is_empty() => {
            Some(vec!["sh".into(), "-c".into(), c.down.clone()])
        }
        _ => None,
    }
}

// ── runtime (I/O seam) ───────────────────────────────────────────────────────

/// Bring up the sidecar: materialize secret files (0600), then `run -d`.
fn run_sidecar(rt: &OciRuntime, plan: &SidecarPlan) -> Result<()> {
    // Already running? `run` would fail on the name clash; treat as idempotent.
    if sidecar_running(rt, &plan.container) {
        return Ok(());
    }
    let mut materialized: Vec<String> = Vec::new();
    let staged = stage_files(plan, &mut materialized)?;

    let mut args: Vec<String> = vec![
        "run".into(),
        "-d".into(),
        "--name".into(),
        plan.container.clone(),
    ];
    args.extend(plan.run_flags.iter().cloned());
    for (k, v) in &plan.env {
        args.push("-e".into());
        args.push(format!("{k}={v}"));
    }
    for (host, dest) in plan.mounts.iter().chain(staged.iter()) {
        args.push("-v".into());
        args.push(format!("{host}:{dest}:ro"));
    }
    args.push(plan.image.clone());
    args.extend(plan.command.iter().cloned());

    let argv = rt.argv(&args.iter().map(|s| s.as_str()).collect::<Vec<_>>());
    let out = std::process::Command::new(&argv[0])
        .args(&argv[1..])
        .output()
        .with_context(|| format!("vpn: start sidecar {}", plan.container))?;
    if !out.status.success() {
        bail!(
            "vpn: sidecar '{}' failed to start: {}",
            plan.container,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Materialize `plan.files` to a 0600 dir under the state home; return their
/// `(host, dest)` mount pairs.
fn stage_files(plan: &SidecarPlan, written: &mut Vec<String>) -> Result<Vec<(String, String)>> {
    if plan.files.is_empty() {
        return Ok(Vec::new());
    }
    let dir = state_dir().join(&plan.container);
    std::fs::create_dir_all(&dir).with_context(|| format!("vpn: mkdir {}", dir.display()))?;
    let mut out = Vec::new();
    for (i, f) in plan.files.iter().enumerate() {
        let host = dir.join(format!("f{i}"));
        std::fs::write(&host, &f.contents)
            .with_context(|| format!("vpn: write {}", host.display()))?;
        set_0600(&host);
        written.push(host.to_string_lossy().into_owned());
        out.push((host.to_string_lossy().into_owned(), f.dest.clone()));
    }
    Ok(out)
}

fn state_dir() -> std::path::PathBuf {
    let base = std::env::var("XDG_STATE_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
            std::path::PathBuf::from(home).join(".local/state")
        });
    base.join("thegn").join("vpn")
}

#[cfg(unix)]
fn set_0600(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}
#[cfg(not(unix))]
fn set_0600(_path: &std::path::Path) {}

/// Is the sidecar already running? (`inspect` exits 0 only for live containers.)
fn sidecar_running(rt: &OciRuntime, container: &str) -> bool {
    let argv = rt.argv(&["inspect", "-f", "{{.State.Running}}", container]);
    std::process::Command::new(&argv[0])
        .args(&argv[1..])
        .output()
        .map(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).trim() == "true")
        .unwrap_or(false)
}

/// `exec` a command inside the sidecar; returns `(success, stdout)`. Public so
/// the share layer can drive `tailscale serve` inside an existing VPN sidecar.
pub fn exec_in(rt: &OciRuntime, container: &str, cmd: &[String]) -> Result<(bool, String)> {
    let mut args = vec!["exec".to_string(), container.to_string()];
    args.extend(cmd.iter().cloned());
    let argv = rt.argv(&args.iter().map(|s| s.as_str()).collect::<Vec<_>>());
    let out = std::process::Command::new(&argv[0])
        .args(&argv[1..])
        .output()
        .with_context(|| format!("vpn: exec in {container}"))?;
    Ok((
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    ))
}

/// Poll the readiness probe until it succeeds or `timeout` elapses.
fn poll_ready(
    rt: &OciRuntime,
    container: &str,
    probe: &ReadyProbe,
    timeout: Duration,
) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok((ok, stdout)) = exec_in(rt, container, &probe.argv) {
            let ready = match &probe.when {
                ReadyWhen::ExitZero => ok,
                ReadyWhen::StdoutContains(s) => stdout.contains(s.as_str()),
            };
            if ready {
                return Ok(());
            }
        }
        if Instant::now() >= deadline {
            bail!(
                "vpn: tunnel '{container}' did not become ready within {}s",
                timeout.as_secs()
            );
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

#[cfg(test)]
mod tests;
