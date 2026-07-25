//! Per-worktree sandbox / container backends.
//!
//! When a worktree pane is about to exec its agent/shell (see `pick_agent`), we
//! optionally wrap that process in a sandbox so a coding agent can't reach the
//! whole host. The worktree itself stays a normal git worktree on the host
//! filesystem — only the *interactive process* runs inside the sandbox, with the
//! worktree (and its repo's git-common dir) **bind-mounted at the same absolute
//! path**. That path-preservation is what keeps git working inside the sandbox: a
//! worktree's `.git` is a file pointing at `<repo>/.git/worktrees/<id>`, so both
//! trees must be visible at their host paths. Because the files live on the host,
//! the host-side sidebar/panel/PR (`git -C <worktree>`) keep working unchanged.
//!
//! Backends form an auto-detect chain (`backend = "auto"`): image-based OCI
//! runtimes (podman/docker, plus apple/wsl stubs) when an `image` is set, else a
//! lightweight namespace sandbox reusing the host toolchain (bwrap/systemd),
//! finally `none` (the plain host shell, with a warning). An orthogonal transport
//! layer (mosh preferred / ssh) runs the whole thing on a remote machine.

use crate::config::{
    CustomVpnConfig, FileAccess, NetbirdConfig, Network, OpenvpnConfig, RemoteTransport,
    SandboxBackend, SandboxConfig, SandboxProfile, TailscaleConfig, VpnConfig, VpnDnsMode, VpnMode,
    VpnOnError, VpnProviderKind, WireguardConfig, ZerotierConfig,
};
use crate::placement::{Placement, RuntimeProbe, SshPlacement, TransportKind};
use crate::remote::GitLoc;
use crate::sandbox_mounts::{
    auto_cache_mounts, default_writable_carveouts, host_toolchain_mounts_ro_home, keep_cfg_mount,
    parse_mount,
};
use crate::{msg, util};
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

/// Ceiling for fast control-plane probes (`image exists`, `container
/// inspect`, health checks). A wedged runtime (stuck podman machine, broken
/// overlay storage) must FAIL the candidate quickly so the backend chain
/// falls through to bwrap/host instead of freezing the caller — pane spawns
/// run on the event loop's critical path.
pub(crate) const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
/// Ceiling for a runtime probe that rides a REMOTE control transport (ssh /
/// kubectl exec / provider exec): the local `PROBE_TIMEOUT` plus a budget for
/// connection setup. Bounded so a hung/black-holed transport fails fast as
/// `Unreachable` (then the retry/chain takes over) instead of blocking the whole
/// sandbox resolution indefinitely — the raw `Command::output()` it replaced had
/// no deadline, which is how one wedged exec turned into a multi-minute stall.
pub(crate) const REMOTE_PROBE_TIMEOUT: Duration = Duration::from_secs(10);
/// Ceiling for container create (`run -d`): image is prefetched by then, so
/// this is namespace/cgroup setup, not network.
pub(crate) const RUN_TIMEOUT: Duration = Duration::from_secs(30);
/// Ceiling for image pulls (network, legitimately slow — but never forever).
pub(crate) const PULL_TIMEOUT: Duration = Duration::from_secs(120);

/// Run `argv` for its exit status with a hard deadline, stdio discarded.
/// `None` on spawn failure or timeout (the child is killed and reaped) — for
/// callers, indistinguishable from "this backend doesn't work", which is
/// exactly the degradation the chain wants.
pub(crate) fn status_with_timeout(argv: &[String], timeout: Duration) -> Option<bool> {
    output_with_timeout(argv, timeout).map(|(ok, _)| ok)
}

/// Like [`status_with_timeout`] but also captures stdout. Returns
/// `(success, stdout)` or `None` on spawn failure or timeout.
pub(crate) fn output_with_timeout(argv: &[String], timeout: Duration) -> Option<(bool, String)> {
    use std::process::Stdio;
    let mut child = Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                // stdout is available once the process has exited.
                let stdout = child
                    .stdout
                    .take()
                    .and_then(|mut r| {
                        use std::io::Read;
                        let mut s = String::new();
                        r.read_to_string(&mut s).ok().map(|_| s)
                    })
                    .unwrap_or_default();
                return Some((status.success(), stdout));
            }
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(25)),
            Err(_) => return None,
        }
    }
}

/// Runtime backend (resolved from the config-facing [`SandboxBackend`]; this set
/// has no `Auto` — auto resolution is what produces a concrete `Backend`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Backend {
    /// Rootless podman (default podman invocation).
    Podman,
    /// Rootful podman via non-interactive sudo (`sudo -n podman`).
    PodmanRootful,
    Docker,
    Smol,
    Bwrap,
    Systemd,
    Apple,
    Wsl,
    WinAppContainer,
    WinJobObject,
    None,
}

impl Backend {
    /// Resolve a config-facing backend name (as used in `backend_chain` entries,
    /// e.g. `"podman-rootless"`, `"bwrap"`, `"host"`) to its concrete runtime
    /// backend. Returns `None` for unknown names.
    pub fn parse(s: &str) -> Option<Backend> {
        Some(match s {
            "podman" | "podman-rootless" | "rootless-podman" => Backend::Podman,
            "podman-rootful" | "rootful-podman" => Backend::PodmanRootful,
            "docker" => Backend::Docker,
            "smol" | "smolmachines" => Backend::Smol,
            "bwrap" | "bubblewrap" => Backend::Bwrap,
            "systemd" | "systemd-run" => Backend::Systemd,
            "apple" | "container" => Backend::Apple,
            "wsl" => Backend::Wsl,
            "winappcontainer" | "appcontainer" => Backend::WinAppContainer,
            "winjobobject" | "jobobject" => Backend::WinJobObject,
            "none" | "host" => Backend::None,
            _ => return None,
        })
    }

    /// Map a config backend to its runtime form. `Auto` has no concrete runtime
    /// backend (it triggers the detection chain) and yields `None`.
    pub fn from_config(b: SandboxBackend) -> Option<Backend> {
        Some(match b {
            SandboxBackend::Auto => return None,
            SandboxBackend::Podman => Backend::Podman,
            SandboxBackend::PodmanRootful => Backend::PodmanRootful,
            SandboxBackend::Docker => Backend::Docker,
            SandboxBackend::Smol => Backend::Smol,
            SandboxBackend::Bwrap => Backend::Bwrap,
            SandboxBackend::Systemd => Backend::Systemd,
            SandboxBackend::Apple => Backend::Apple,
            SandboxBackend::Wsl => Backend::Wsl,
            SandboxBackend::WinAppContainer => Backend::WinAppContainer,
            SandboxBackend::WinJobObject => Backend::WinJobObject,
            SandboxBackend::None => Backend::None,
        })
    }

    /// The executable to probe / invoke for this backend.
    pub fn label(self) -> &'static str {
        match self {
            Backend::Podman => "podman-rootless",
            Backend::PodmanRootful => "podman-rootful",
            Backend::Docker => "docker",
            Backend::Smol => "smolmachines",
            Backend::Bwrap => "bwrap",
            Backend::Systemd => "systemd",
            Backend::Apple => "apple",
            Backend::Wsl => "wsl",
            Backend::WinAppContainer => "appcontainer",
            Backend::WinJobObject => "jobobject",
            Backend::None => "host",
        }
    }

    pub fn binary(self) -> &'static str {
        match self {
            Backend::Podman | Backend::PodmanRootful => "podman",
            Backend::Docker => "docker",
            Backend::Smol => "smolmachines",
            Backend::Bwrap => "bwrap",
            Backend::Systemd => "systemd-run",
            Backend::Apple => "container",
            Backend::Wsl => "wsl.exe",
            Backend::WinAppContainer | Backend::WinJobObject => "", // OS native
            Backend::None => "",
        }
    }

    /// OCI runtimes consume an image and keep a persistent named container per
    /// worktree; the others reuse the host toolchain per pane.
    pub fn is_oci(self) -> bool {
        matches!(
            self,
            Backend::Podman
                | Backend::PodmanRootful
                | Backend::Docker
                | Backend::Smol
                | Backend::Apple
                | Backend::Wsl
        )
    }

    pub fn is_host_toolchain(self) -> bool {
        matches!(
            self,
            Backend::Bwrap | Backend::Systemd | Backend::WinAppContainer | Backend::WinJobObject
        )
    }
}

// The execution placement (`Local | Ssh | K8s | Provider`) and its exec-wrapping
// logic now live in `crate::placement`. `SandboxSpec` carries a resolved
// `Placement`; `enter_argv`/`control_argv` delegate the outer wrap to it.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mount {
    pub host: String,
    pub dest: String,
    pub ro: bool,
    pub cache: bool,
}

/// Resolved per-pane / aggregate ceilings for a spec. See
/// [`crate::sandbox_cpucap`] for how each field maps onto the backend argv.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SandboxLimits {
    /// Per-pane CPU ceiling, in cores. OCI `--cpus`; `CPUQuota` elsewhere.
    pub cpu: Option<String>,
    /// Per-pane memory ceiling (`"512m"`, `"4g"`). `--memory` / `MemoryMax`.
    pub memory: Option<String>,
    /// Aggregate CPU ceiling across all panes (cores). `None` = auto.
    pub cpu_total: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SandboxSpec {
    pub backend: Backend,
    pub placement: Placement,
    pub image: Option<String>,
    pub worktree: PathBuf,
    pub mounts: Vec<Mount>,
    pub env: Vec<(String, String)>,
    /// Per-agent env overrides: injected into the shell script before the inner
    /// command runs, taking priority over env_passthrough. Used for scoped API
    /// keys (virtual keys from the LLM proxy) without rebuilding the container.
    pub env_overrides: std::collections::HashMap<String, String>,
    /// Env keys to suppress inside the sandbox — unset even if forwarded by
    /// env_passthrough or present in the OCI image. Use alongside env_overrides
    /// to swap a master key for a scoped virtual key.
    pub env_block: Vec<String>,
    pub network: Network,
    /// Domain allow-list for the DNS filter (empty = allow all non-blocked).
    pub network_allow: Vec<String>,
    /// Domain block-list for the DNS filter (checked before allow-list).
    pub network_block: Vec<String>,
    /// Hardening: mount the container root filesystem read-only (writable: the
    /// worktree, cache binds, and a tmpfs `/tmp`). Resolved from the active
    /// `SandboxProfile`.
    pub read_only_root: bool,
    /// Hardening: set `no-new-privileges` so setuid/setgid can't escalate.
    pub no_new_privileges: bool,
    /// Hardening: process cap (fork-bomb containment); `None` = unlimited.
    pub pids_limit: Option<i64>,
    /// Hardening: Linux capabilities to drop (e.g. `["ALL"]` for `sealed`).
    pub drop_capabilities: Vec<String>,
    /// Hardening: capabilities to add back after dropping.
    pub add_capabilities: Vec<String>,
    pub file_access: FileAccess,
    pub ports: Vec<String>,
    pub gpu: Option<String>,
    pub limits: SandboxLimits,
    pub volumes: Vec<(String, String)>,
    pub compose: Option<String>,
    /// Programmatic Dockerfile build (from a devcontainer `build` block). When
    /// set, [`ensure`] builds the image to `image` before create. See
    /// [`crate::sandbox_build`].
    pub build: Option<crate::sandbox_build::SandboxBuild>,
    pub init_script: Option<String>,
    pub devenv: bool,
    /// Absolute path to the `devenv` binary on the host (resolved at spec-build
    /// time when `devenv = true`). Used in `wrap_script` so OCI containers don't
    /// rely on `devenv` being on their PATH.
    pub devenv_path: Option<String>,
    pub name: String,
    /// Resolved VPN/tunnel attachment for this sandbox, or `None` when no tunnel
    /// is requested (or it was refused by the active profile). Pure data — the
    /// behavior (bring-up, readiness, teardown) lives in `thegn-svc::vpn`.
    pub vpn: Option<VpnSpec>,
    /// Remote OCI daemon to drive (`[sandbox] oci_host`): a podman connection
    /// URL/name or docker host. `None` ⇒ the local daemon. Injected before every
    /// container subcommand by `oci_prefix`.
    pub oci_host: Option<String>,
}

impl SandboxSpec {
    /// The aggregated [`Capabilities`](crate::capabilities::Capabilities) of this
    /// resolved spec — what it can project/enforce/observe/snapshot — so callers
    /// ask one value instead of re-deriving `is_oci`/profile/placement booleans.
    pub fn capabilities(&self) -> crate::capabilities::Capabilities {
        crate::capabilities::Capabilities::derive(self)
    }

    /// The decoded compose declaration, if this spec is compose-backed.
    pub fn compose_spec(&self) -> Option<crate::sandbox_compose::ComposeSpec> {
        self.compose
            .as_deref()
            .map(crate::sandbox_compose::ComposeSpec::decode)
    }
}

/// A resolved, identity-bearing VPN attachment request for one sandbox. Pure
/// data assembled by [`build_vpn_spec`]; secrets-refs in `params` are left
/// **unresolved** here and dereferenced only at bring-up time in `thegn-svc`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VpnSpec {
    pub provider: VpnProviderKind,
    pub mode: VpnMode,
    pub on_error: VpnOnError,
    pub dns_mode: VpnDnsMode,
    pub ready_timeout: Duration,
    /// Request an ephemeral node identity (auto-deregisters on teardown) where
    /// the provider supports it.
    pub ephemeral: bool,
    /// Sidecar image override; `None` = the provider's default image.
    pub sidecar_image: Option<String>,
    /// Node/peer name in the overlay (defaults to the container name).
    pub hostname: String,
    /// The selected provider's configuration (still carrying secrets-refs).
    pub params: VpnParams,
}

/// Provider-specific VPN parameters, mirroring the `[sandbox.vpn.<provider>]`
/// sub-tables. Headscale reuses [`VpnParams::Tailscale`] (the `provider` field
/// on [`VpnSpec`] distinguishes them; Headscale just requires `login_server`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VpnParams {
    Tailscale(TailscaleConfig),
    Wireguard(WireguardConfig),
    Openvpn(OpenvpnConfig),
    Netbird(NetbirdConfig),
    Zerotier(ZerotierConfig),
    Custom(CustomVpnConfig),
}

/// Resolve a `[sandbox.vpn]` config block into a [`VpnSpec`] for the worktree
/// container named `name`, reconciling with the hardening `profile`.
///
/// Returns `None` when no provider is configured, or when the active profile
/// refuses a tunnel (plain `sealed`: a tunnel would contradict its no-network /
/// no-caps contract — the user is told to use `sealed-tunnel` or `hardened`).
pub fn build_vpn_spec(cfg: &VpnConfig, name: &str, profile: SandboxProfile) -> Option<VpnSpec> {
    if !cfg.is_enabled() {
        return None;
    }
    if !profile.permits_vpn() {
        msg::warn(&format!(
            "sandbox: profile '{profile}' forbids a VPN tunnel (network=none, all \
             capabilities dropped); ignoring [sandbox.vpn]. Use 'sealed-tunnel' for a \
             tunnel-only worktree, or 'hardened'.",
        ));
        return None;
    }
    let params = match cfg.provider {
        VpnProviderKind::None => return None,
        VpnProviderKind::Tailscale | VpnProviderKind::Headscale => {
            VpnParams::Tailscale(cfg.tailscale.clone())
        }
        VpnProviderKind::Wireguard => VpnParams::Wireguard(cfg.wireguard.clone()),
        VpnProviderKind::Openvpn => VpnParams::Openvpn(cfg.openvpn.clone()),
        VpnProviderKind::Netbird => VpnParams::Netbird(cfg.netbird.clone()),
        VpnProviderKind::Zerotier => VpnParams::Zerotier(cfg.zerotier.clone()),
        VpnProviderKind::Custom => VpnParams::Custom(cfg.custom.clone()),
    };
    // A per-provider hostname overrides the container-name default.
    let hostname = match &params {
        VpnParams::Tailscale(t) if !t.hostname.trim().is_empty() => t.hostname.clone(),
        VpnParams::Netbird(n) if !n.hostname.trim().is_empty() => n.hostname.clone(),
        _ => name.to_string(),
    };
    Some(VpnSpec {
        provider: cfg.provider,
        mode: cfg.mode,
        on_error: cfg.on_error,
        dns_mode: cfg.dns,
        ready_timeout: Duration::from_secs(cfg.ready_timeout_secs),
        ephemeral: cfg.ephemeral,
        sidecar_image: {
            let t = cfg.sidecar_image.trim();
            (!t.is_empty()).then(|| t.to_string())
        },
        hostname,
        params,
    })
}

/// Build the sandbox spec for a worktree (described by its `GitLoc`), or `None`
/// to run on the host (sandbox disabled, or the chain resolved to `none`). The
/// location drives both remote-ness (transport) and how git metadata is probed.
/// Emits a warning when it falls back per `on_missing`.
pub fn resolve(cfg: &SandboxConfig, loc: &GitLoc, name: &str) -> Option<SandboxSpec> {
    resolve_scoped(cfg, loc, name, cfg.profile)
}

/// Like [`resolve`] but with an explicit hardening [`SandboxProfile`]. Used for
/// the embedded agent's separate `agent_profile` container, which is sealed
/// independently of the worktree's interactive `profile`.
pub fn resolve_scoped(
    cfg: &SandboxConfig,
    loc: &GitLoc,
    name: &str,
    profile: SandboxProfile,
) -> Option<SandboxSpec> {
    let placement = placement_from_loc(cfg, loc);
    resolve_placed(cfg, loc, name, profile, placement)
}

/// Like [`resolve_scoped`] but with an explicit [`Placement`]. This is the seam
/// the named-environment layer ([`crate::env`]) drives: it resolves where a
/// worktree runs (local / ssh / k8s / provider) and hands the placement in,
/// instead of letting `[sandbox.remote]` + the `GitLoc` decide. The default
/// callers ([`resolve`]/[`resolve_scoped`]) derive the placement from the loc so
/// existing behavior is unchanged.
pub fn resolve_placed(
    cfg: &SandboxConfig,
    loc: &GitLoc,
    name: &str,
    profile: SandboxProfile,
    placement: Placement,
) -> Option<SandboxSpec> {
    if !cfg.enabled {
        return None;
    }
    let backend = pick_backend(cfg, &placement)?;
    // `none` on a *local* worktree means "run on the host" (caller's plain-shell
    // fallback). For a *remote* placement we still need it to carry a bare shell
    // to the target, so keep building the spec.
    if backend == Backend::None && placement.is_local() {
        return None;
    }

    let image = {
        let t = cfg.image.trim();
        if t.is_empty() {
            None
        } else {
            Some(t.to_string())
        }
    };
    let worktree = PathBuf::from(loc.path());
    // Path-preserving git mounts: the worktree and the repo's git-common dir
    // (probed via the location, so it's the *remote* path for remote worktrees).
    let git_common = loc
        .git_out(&["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .map(PathBuf::from)
        .filter(|p| p.as_path() != worktree && !worktree.starts_with(p));

    let mut mounts = vec![];
    let add_worktree_mounts = |mounts: &mut Vec<Mount>| {
        mounts.push(Mount {
            host: loc.path(),
            dest: loc.path(),
            ro: false,
            cache: false,
        });
        if let Some(gc) = &git_common {
            let g = gc.to_string_lossy().into_owned();
            mounts.push(Mount {
                host: g.clone(),
                dest: g.clone(),
                ro: false,
                cache: false,
            });
            // Pin the SHARED `.git/config` read-only on top of the writable
            // `.git`: objects/refs/index (and per-worktree config under
            // `worktrees/<name>/`) stay writable so commits work, but no
            // sandboxed process can write a stray `core.worktree`/`user.*` into
            // the shared config — the structural fix for the pollution class.
            // Emitted AFTER the parent bind so the sub-path override wins
            // (bwrap `--ro-bind`, OCI file-level `:ro`).
            let cfg = format!("{g}/config");
            if std::path::Path::new(&cfg).exists() {
                mounts.push(Mount {
                    host: cfg.clone(),
                    dest: cfg,
                    ro: true,
                    cache: false,
                });
            }
        }
    };
    // Inject host toolchain paths (dotfiles, $HOME, /nix/store, etc.) so the
    // user's real shell, starship, and configs work identically in the sandbox.
    //
    // OCI (podman/docker): container image has none of the host paths — we must
    //   mount everything in explicitly.
    // bwrap: hardcodes /nix/store, /usr, /etc in backend_enter_argv, but does
    //   NOT include $HOME, so dotfiles (.zshrc, .config/starship.toml) are
    //   absent and zsh runs zsh-newuser-install instead of the real config.
    //   host_toolchain_mounts() fills in $HOME and other user-specific paths;
    //   bwrap picks them up via spec.mounts → --ro-bind flags.
    // systemd/host: full host filesystem, no extra mounts needed.
    let inject_host_toolchain = (backend.is_oci() || backend == Backend::Bwrap) && cfg.auto_caches;
    // Read-only-outside-the-worktree by default: mount $HOME read-only unless the
    // profile explicitly opts out. OCI always mounts home ro (root in a foreign
    // image, must not write). bwrap/systemd honor the hardening profile: the
    // default `hardened`/`sealed` (read_only_root) → ro $HOME so a sandboxed
    // agent can't `cd` out of the worktree and modify/delete host files;
    // `profile = "open"` → rw $HOME as the escape hatch. Writes tools genuinely
    // need (zsh history, zoxide, atuin) are carved back narrowly below.
    let home_ro = backend.is_oci() || profile.read_only_root();

    // Emit the host-toolchain substrate (ro $HOME) BEFORE the worktree/caches so
    // the read-write worktree, git dir, and caches — which live *under* $HOME —
    // overmount the read-only $HOME parent (bwrap applies binds in order; a later
    // child bind wins). Same mechanism as the `.git`(rw) → `.git/config`(ro) pin.
    match cfg.file_access {
        FileAccess::All | FileAccess::Host => {
            mounts.push(Mount {
                host: "/".into(),
                dest: "/".into(),
                ro: false,
                cache: false,
            });
        }
        FileAccess::Worktree => {
            if inject_host_toolchain {
                mounts.extend(host_toolchain_mounts_ro_home(home_ro));
            }
            add_worktree_mounts(&mut mounts);
        }
        FileAccess::WorktreePlusCaches => {
            if inject_host_toolchain {
                mounts.extend(host_toolchain_mounts_ro_home(home_ro));
            }
            add_worktree_mounts(&mut mounts);
            if cfg.auto_caches {
                mounts.extend(auto_cache_mounts());
            }
        }
        FileAccess::Custom => add_worktree_mounts(&mut mounts),
        FileAccess::None => {}
    }

    // Under a read-only $HOME, carve narrow read-write paths back so shell/tool
    // state (history, zoxide, atuin) and a personal scratch dir keep working.
    // Only when $HOME was actually injected read-only: `all`/`host` are already
    // fully writable, and `custom`/`none` withhold $HOME entirely.
    if inject_host_toolchain && home_ro {
        for cv in default_writable_carveouts(profile) {
            if keep_cfg_mount(&mounts, &cv) {
                mounts.push(cv);
            }
        }
    }

    for m in &cfg.mounts {
        let parsed = parse_mount(m);
        // Skip mounts whose source doesn't exist — silently, since config
        // defaults like ~/.gitconfig may not be present on every machine.
        if !std::path::Path::new(&parsed.host).exists() {
            continue;
        }
        // A read-write *directory* under a read-only parent (e.g. `~/.gnupg` under
        // the read-only $HOME) is kept — it overmounts the parent read-write. A
        // read-only entry already covered, an exact duplicate, or a *file*
        // mount-point inside a bound dir (bwrap "Can't create file", e.g.
        // `~/.gitconfig`) is skipped. See `keep_cfg_mount`.
        if keep_cfg_mount(&mounts, &parsed) {
            mounts.push(parsed);
        }
    }

    // SSH identity keys: secret managers (agenix, sops-nix) keep private keys
    // on a tmpfs outside $HOME — `~/.ssh/id_*` are symlinks into trees we
    // deliberately do NOT bind wholesale. Bind just the referenced key files
    // read-only at their symlink-target paths so the $HOME-mounted symlinks
    // resolve in-sandbox. Local placements only (paths are probed on this
    // host); gated exactly like the toolchain mounts that expose $HOME.
    if inject_host_toolchain && placement.is_local() {
        let mut covered: Vec<String> = mounts.iter().map(|m| m.dest.clone()).collect();
        if backend == Backend::Bwrap {
            covered.extend(BWRAP_SUBSTRATE.iter().map(|s| s.to_string()));
        }
        mounts.extend(crate::ssh_creds::identity_mounts(&covered));
    }

    let mut env: Vec<(String, String)> = cfg
        .env_passthrough
        .iter()
        .filter_map(|k| std::env::var(k).ok().map(|v| (k.clone(), v)))
        // A dead agent socket is worse than none: every in-sandbox ssh would
        // waste a connect on it (and `AddKeysToAgent` errors per connection).
        .filter(|(k, v)| k != "SSH_AUTH_SOCK" || crate::ssh_creds::unix_socket_alive(v))
        .collect();

    // Marker so a pane's shell rc / tooling can tell it's running inside a
    // thegn sandbox (e.g. to skip a redundant in-sandbox `direnv` hook).
    // Emitted via the backend's env mechanism (`--setenv`/`-e`); host-fallback
    // panes carry no spec and so stay unmarked — correct, they aren't sandboxed.
    env.push(("THEGN_SANDBOX".to_string(), "1".to_string()));

    // Tier B: expose the host Nix daemon inside the sandbox so full
    // `nix develop`/`build`/`fmt` work there (Tier A only gives read-only tools
    // on PATH). Path-preserving bind of the daemon-socket dir + `NIX_REMOTE`;
    // the daemon mediates store writes, so the read-only `/nix/store` mount is
    // fine. `nix_daemon = true` forces it on for every sandbox; otherwise it's a
    // backstop auto-enabled for a local flake-backed worktree so an in-sandbox
    // `nix-direnv` cache MISS re-evals via the daemon instead of dying on the
    // read-only `/nix/store` (see [`crate::direnv`]). Opt out with
    // `warm_direnv = off` (disables the whole in-sandbox-direnv machinery) or
    // `profile = sealed` (no-network floor). The socket is a local unix socket,
    // so this is compatible with `network = none`.
    let auto_daemon = placement.is_local()
        && !profile.forces_no_network()
        && cfg.warm_direnv != crate::config::WarmDirenv::Off
        && crate::direnv::has_flake_envrc(&worktree);
    if cfg.nix_daemon || auto_daemon {
        const SOCK_DIR: &str = "/nix/var/nix/daemon-socket";
        if std::path::Path::new(SOCK_DIR).join("socket").exists() {
            mounts.push(Mount {
                host: SOCK_DIR.to_string(),
                dest: SOCK_DIR.to_string(),
                ro: true,
                cache: false,
            });
            env.push(("NIX_REMOTE".to_string(), "daemon".to_string()));
        } else if cfg.nix_daemon {
            // Only nag when explicitly requested; the auto backstop stays silent.
            msg::warn(
                "sandbox: [sandbox] nix_daemon is on but no host Nix daemon socket was \
                 found (/nix/var/nix/daemon-socket/socket); leaving it off. Tier A \
                 (inject_devshell) still provides the devShell tools read-only.",
            );
        }
    }

    Some(SandboxSpec {
        backend,
        placement,
        image,
        worktree,
        mounts,
        env,
        env_overrides: std::collections::HashMap::new(),
        // Strip the repo-targeting git env inside the sandbox: bwrap/systemd
        // inherit the host env, so an `unset GIT_DIR …` at the top of the wrapped
        // script ensures a sandboxed shell/agent can't be misdirected at the
        // shared `.git` (defense in depth atop the read-only `.git/config` mount).
        env_block: crate::util::GIT_ENV_VARS
            .iter()
            .map(|s| s.to_string())
            .collect(),
        // A profile with a no-network floor (sealed) overrides the configured
        // network mode; otherwise the worktree's `[sandbox] network` stands.
        network: if profile.forces_no_network() {
            Network::None
        } else {
            cfg.network
        },
        network_allow: cfg.network_allow.clone(),
        network_block: cfg.network_block.clone(),
        read_only_root: profile.read_only_root(),
        no_new_privileges: profile.no_new_privileges(),
        pids_limit: profile.pids_limit(),
        drop_capabilities: profile.drop_capabilities(),
        add_capabilities: profile.add_capabilities(),
        file_access: cfg.file_access,
        ports: cfg.ports.clone(),
        gpu: cfg.gpu.clone(),
        limits: SandboxLimits {
            cpu: cfg.limits.cpu.clone(),
            memory: cfg.limits.memory.clone(),
            cpu_total: cfg.limits.cpu_total.clone(),
        },
        volumes: cfg
            .volumes
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        compose: cfg.compose.clone(),
        build: cfg.build.clone(),
        init_script: (!cfg.init_script.trim().is_empty()).then(|| cfg.init_script.clone()),
        // Explicit opt-in, or an OCI-backed local repo with devenv.nix when
        // `devenv` is on PATH.  Auto-detection is OCI-only: for bwrap/systemd
        // the host toolchain is already mounted and the user's login shell
        // already activates the devenv env — running `devenv shell` inside
        // bwrap would fail because the Nix daemon socket is not mounted there.
        devenv: cfg.devenv
            || (backend.is_oci()
                && !loc.is_remote()
                && PathBuf::from(loc.path()).join("devenv.nix").is_file()
                && util::have("devenv")),
        // Resolve the absolute devenv path at spec-build time so OCI containers
        // (which don't inherit the host PATH) can still exec it directly.
        devenv_path: util::which_path("devenv"),
        name: name.to_string(),
        vpn: {
            if cfg.vpn.is_enabled() && cfg.network == Network::Host && !profile.forces_no_network()
            {
                msg::warn(
                    "sandbox: [sandbox] network=host conflicts with a VPN tunnel \
                     (host networking is what the tunnel isolates from); the worktree \
                     will join the tunnel instead of sharing the host network.",
                );
            }
            build_vpn_spec(&cfg.vpn, name, profile)
        },
        oci_host: (!cfg.oci_host.trim().is_empty()).then(|| cfg.oci_host.trim().to_string()),
    })
}

/// Name prefix for every container thegn creates (per-worktree sandboxes).
pub const CONTAINER_PREFIX: &str = "thegn-";

/// The deterministic per-worktree container name, derived from the worktree path
/// so the create site (pick_agent) and `teardown` (close_worktree) always agree —
/// local or remote, no DB slug lookup needed.
pub fn container_name(worktree: &str) -> String {
    format!("{CONTAINER_PREFIX}{}", util::slugify(worktree))
}

/// Per-profile variant: `thegn-{profile}-{slug}` when a profile is active.
/// Falls back to [`container_name`] when `profile` is `None` or `"default"`.
pub fn container_name_with_profile(worktree: &str, profile: Option<&str>) -> String {
    match profile {
        Some(p) if !p.is_empty() && p != "default" => {
            format!(
                "{CONTAINER_PREFIX}{}-{}",
                util::slugify(p),
                util::slugify(worktree)
            )
        }
        _ => container_name(worktree),
    }
}

/// Suffix marking the embedded agent's own (separately-hardened) container, used
/// when `agent_profile` differs from the worktree `profile` so the agent runs in
/// a more-locked-down container than the interactive shell. Chosen to be
/// collision-resistant against worktree slugs that happen to end in `-agent`.
pub const AGENT_CONTAINER_SUFFIX: &str = "-szagent";

/// The agent's container name, derived from the worktree container name `base`.
pub fn agent_container_name(base: &str) -> String {
    format!("{base}{AGENT_CONTAINER_SUFFIX}")
}

/// Strip [`AGENT_CONTAINER_SUFFIX`] so reverse lookups (orphan reconciliation,
/// event→worktree mapping) treat the agent container as its worktree's.
pub fn strip_agent_suffix(name: &str) -> &str {
    name.strip_suffix(AGENT_CONTAINER_SUFFIX).unwrap_or(name)
}

/// Suffix marking a worktree's VPN sidecar — the companion container that owns
/// the tunnel's network namespace (the worktree container joins it via
/// `--network container:<sidecar>`). Deterministic from the worktree container
/// name so the bring-up (`thegn-svc::vpn`), the `--network` wiring
/// (`oci_create_opts`), and teardown all agree without a registry lookup.
pub const VPN_SIDECAR_SUFFIX: &str = "-szvpn";

/// The VPN sidecar container name, derived from the worktree container name `base`.
pub fn vpn_sidecar_name(base: &str) -> String {
    format!("{base}{VPN_SIDECAR_SUFFIX}")
}

/// Strip [`VPN_SIDECAR_SUFFIX`] so orphan reconciliation maps a stray sidecar
/// back to its worktree.
pub fn strip_vpn_suffix(name: &str) -> &str {
    name.strip_suffix(VPN_SIDECAR_SUFFIX).unwrap_or(name)
}

/// One running container, as listed by the OCI runtime — feeds the panel's
/// SANDBOXES section. `ours` marks thegn-created (prefix-named) ones.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerInfo {
    pub name: String,
    pub image: String,
    pub status: String,
    pub ours: bool,
    pub backend: String,
    pub cpu: String,
    pub mem: String,
    pub net: String,
    pub containment: String,
    pub mounts: String,
}

fn container_info(name: String, image: String, status: String, backend: &str) -> ContainerInfo {
    let ours = name.starts_with(CONTAINER_PREFIX);
    ContainerInfo {
        name,
        image,
        status,
        ours,
        backend: backend.to_string(),
        cpu: String::new(),
        mem: String::new(),
        net: String::new(),
        containment: "worktree+caches".into(),
        mounts: String::new(),
    }
}

/// Parse `podman ps --format json` (one JSON array; `Names` is a list).
pub fn parse_podman_ps(json: &str) -> Vec<ContainerInfo> {
    let Ok(rows) = serde_json::from_str::<Vec<serde_json::Value>>(json) else {
        return Vec::new();
    };
    rows.into_iter()
        .filter_map(|r| {
            let name = r.get("Names")?.as_array()?.first()?.as_str()?.to_string();
            let image = r.get("Image").and_then(|v| v.as_str()).unwrap_or("").into();
            let status = r
                .get("Status")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .into();
            Some(container_info(name, image, status, "podman"))
        })
        .collect()
}

/// Parse `docker ps --format '{{json .}}'` (NDJSON; `Names` is a string).
pub fn parse_docker_ps(ndjson: &str) -> Vec<ContainerInfo> {
    ndjson
        .lines()
        .filter_map(|line| {
            let r: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
            let name = r.get("Names")?.as_str()?.to_string();
            let image = r.get("Image").and_then(|v| v.as_str()).unwrap_or("").into();
            let status = r
                .get("Status")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .into();
            Some(container_info(name, image, status, "docker"))
        })
        .collect()
}

/// The running containers, thegn-owned first. Probes rootless podman,
/// rootful podman, then docker; one fast subprocess on the caller's
/// (background) thread. Empty when no OCI runtime is installed.
pub fn running_containers() -> Vec<ContainerInfo> {
    let mut out = Vec::new();
    if let Some(stdout) = run_local_output(
        &backend_prefix(Backend::Podman),
        &["ps", "--format", "json"],
    ) {
        let mut rows = parse_podman_ps(&stdout);
        apply_stats(&mut rows, &oci_stats(Backend::Podman));
        out.extend(rows);
    }
    if let Some(stdout) = run_local_output(
        &backend_prefix(Backend::PodmanRootful),
        &["ps", "--format", "json"],
    ) {
        let mut rows = parse_podman_ps(&stdout);
        for r in &mut rows {
            r.backend = "podman-rootful".into();
        }
        apply_stats(&mut rows, &oci_stats(Backend::PodmanRootful));
        out.extend(rows);
    }
    if out.is_empty()
        && let Some(stdout) = run_local_output(
            &backend_prefix(Backend::Docker),
            &["ps", "--format", "{{json .}}"],
        )
    {
        let mut rows = parse_docker_ps(&stdout);
        apply_stats(&mut rows, &oci_stats(Backend::Docker));
        out.extend(rows);
    }
    out.sort_by_key(|c| (!c.ours, c.name.clone()));
    out
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ContainerStat {
    pub cpu: String,
    pub mem: String,
    pub net: String,
}

fn apply_stats(
    rows: &mut [ContainerInfo],
    stats: &std::collections::HashMap<String, ContainerStat>,
) {
    for r in rows {
        if let Some(st) = stats.get(&r.name) {
            r.cpu = st.cpu.clone();
            r.mem = st.mem.clone();
            r.net = st.net.clone();
        }
    }
}

fn oci_stats(backend: Backend) -> std::collections::HashMap<String, ContainerStat> {
    let mut map = std::collections::HashMap::new();
    let Some(stdout) = run_local_output(
        &backend_prefix(backend),
        &["stats", "--no-stream", "--format", "json"],
    ) else {
        return map;
    };
    for (name, st) in parse_stats_rows(&stdout) {
        map.insert(name, st);
    }
    map
}

pub fn parse_stats_rows(output: &str) -> Vec<(String, ContainerStat)> {
    let parse_one = |v: serde_json::Value| -> Option<(String, ContainerStat)> {
        let name = v
            .get("Name")
            .or_else(|| v.get("Names"))?
            .as_str()?
            .to_string();
        let cpu = v
            .get("CPUPerc")
            .or_else(|| v.get("CPU"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let mem = v
            .get("MemUsage")
            .or_else(|| v.get("Mem"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .split('/')
            .next()
            .unwrap_or("")
            .trim()
            .to_string();
        let net = v
            .get("NetIO")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        Some((name, ContainerStat { cpu, mem, net }))
    };
    if let Ok(rows) = serde_json::from_str::<Vec<serde_json::Value>>(output) {
        return rows.into_iter().filter_map(parse_one).collect();
    }
    output
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line.trim()).ok())
        .filter_map(parse_one)
        .collect()
}

/// Derive the default [`Placement`] from `[sandbox.remote]` + the worktree's
/// `GitLoc` — `Local`, or an `Ssh` target (a remote worktree's own ssh target
/// wins over the configured `[sandbox.remote] host`). The named-environment
/// layer bypasses this with [`resolve_placed`] when an env selects k8s/provider.
pub fn placement_from_loc(cfg: &SandboxConfig, loc: &GitLoc) -> Placement {
    let kind = match cfg.remote.transport {
        RemoteTransport::Ssh => TransportKind::Ssh,
        RemoteTransport::Mosh => TransportKind::Mosh,
    };
    if let Some(ssh) = loc.ssh() {
        Placement::Ssh(SshPlacement::plain(
            ssh.host.clone(),
            ssh.port,
            ssh.forward_agent,
            kind,
        ))
    } else if cfg.remote.is_remote() {
        Placement::Ssh(SshPlacement::plain(
            cfg.remote.host.clone(),
            cfg.remote.port,
            cfg.remote.forward_agent,
            kind,
        ))
    } else {
        Placement::Local
    }
}

pub use crate::sandbox_backend::{ProbePass, placement_reachable, probe_pass_guard};
use crate::sandbox_backend::{available, pick_backend};

pub const DEFAULT_OCI_IMAGE: &str = "docker.io/library/debian:stable";

pub(crate) fn effective_image(spec: &SandboxSpec) -> String {
    spec.image
        .clone()
        .unwrap_or_else(|| DEFAULT_OCI_IMAGE.to_string())
}

pub fn health_check(spec: &SandboxSpec) -> bool {
    if !spec.backend.is_oci() {
        return true;
    }
    // Verify both liveness AND that all required bind-mounts are present.
    let (running, mounts_ok) = container_status(spec);
    running && mounts_ok
}

/// Check whether the named container is running AND has all the bind-mounts
/// the spec requires. Returns `(running, mounts_ok)`.
///
/// Uses a single `inspect --format` call (one subprocess, `PROBE_TIMEOUT`
/// bound) for both questions. The format emits an `OK` sentinel first line so
/// we can distinguish "container missing / inspect failed" (no sentinel) from
/// "running but mounts differ".
/// Whether an OCI create (`oci_create_opts`) actually emits a `-v` bind for this
/// mount. The DNS/hosts files are deliberately skipped for OCI backends (the
/// runtime synthesizes its own — see `oci_create_opts`), so `container_status`
/// must use the *same* predicate when it verifies the running container's binds,
/// or it demands mounts that were never created and force-recreates forever.
fn oci_emits_mount(m: &Mount) -> bool {
    !matches!(m.dest.as_str(), "/etc/resolv.conf" | "/etc/hosts")
}

fn container_status(spec: &SandboxSpec) -> (bool, bool) {
    let required: std::collections::HashSet<&str> = spec
        .mounts
        .iter()
        .filter(|m| oci_emits_mount(m))
        .map(|m| m.host.as_str())
        .collect();

    // Emit "RUNNING" if actually running (not "created"/"exited"), then one
    // bind-mount source per line. A container in "created" state passes inspect
    // but cannot accept exec sessions — we must not treat it as healthy.
    let fmt = "{{if .State.Running}}RUNNING{{end}}\n{{range .Mounts}}{{if eq .Type \"bind\"}}{{.Source}}\n{{end}}{{end}}";
    let mut argv = oci_prefix(spec);
    // For remote worktrees the transport wraps the argv; for local we call
    // podman/docker directly. run_control_t_owned gives us the timeout but
    // discards stdout, so we use output_with_timeout for local transport and
    // fall back to run_control_owned (exit-code only → assume stale) for remote.
    if spec.placement.is_local() {
        argv.extend([
            "container".into(),
            "inspect".into(),
            "--format".into(),
            fmt.to_string(),
            spec.name.clone(),
        ]);
        let (ok, stdout) = match output_with_timeout(&argv, PROBE_TIMEOUT) {
            Some(r) => r,
            None => return (false, false), // timed out
        };
        if !ok && stdout.is_empty() {
            return (false, false); // container doesn't exist
        }
        let mut lines = stdout.lines();
        // First line must be "RUNNING" — "CREATED" / "EXITED" / missing → not usable.
        if lines.next() != Some("RUNNING") {
            return (false, false);
        }
        let active: std::collections::HashSet<&str> = lines.filter(|l| !l.is_empty()).collect();
        let mounts_ok = required.iter().all(|r| active.contains(r));
        (true, mounts_ok)
    } else {
        // Remote: run the same inspect command over SSH to verify mounts.
        let mut remote_argv = oci_prefix(spec);
        remote_argv.extend([
            "container".into(),
            "inspect".into(),
            "--format".into(),
            fmt.to_string(),
            spec.name.clone(),
        ]);
        let Some((_, stdout)) = output_control_owned(spec, &remote_argv, PROBE_TIMEOUT) else {
            return (false, false);
        };
        let mut lines = stdout.lines();
        if lines.next() != Some("RUNNING") {
            return (false, false);
        }
        let active: std::collections::HashSet<&str> = lines.filter(|l| !l.is_empty()).collect();
        let mounts_ok = required.iter().all(|r| active.contains(r));
        (true, mounts_ok)
    }
}

/// Ensure any persistent state exists (OCI: a keep-alive container we `exec`
/// into). No-op for host-toolchain backends and `none`.
pub fn ensure(spec: &SandboxSpec) -> anyhow::Result<()> {
    if !spec.backend.is_oci() {
        return Ok(());
    }

    if let Some(compose) = spec.compose_spec() {
        // `docker compose -f … -p <name> up -d [service runServices…]`. The
        // project name is the sandbox name, so the pane's `compose exec` (see
        // `enter_argv`) targets the same project.
        let argv = crate::sandbox_compose::up_argv(&spec.name, &compose);
        let _ = std::process::Command::new(&argv[0])
            .args(&argv[1..])
            .output()?;
        return Ok(());
    }

    // A local Dockerfile build (devcontainer `build`) produces a tag no registry
    // has — skip the pull-prefetch; it's built below, right before create.
    if spec.build.is_none() {
        crate::sandbox_prefetch::prefetch_image(spec)?;
    }

    let rt = spec.backend.binary();

    // Single inspect call: are we running, and do the mounts match?
    let (running, mounts_ok) = container_status(spec);
    if running {
        if mounts_ok {
            return Ok(()); // already running with the correct mounts
        }
        // Stale mounts (e.g. host_toolchain_mounts() added /nix/store after
        // an upgrade) — force-remove and fall through to recreate.
        msg::warn(&format!(
            "sandbox: container '{}' has stale mounts (config changed); recreating",
            spec.name
        ));
        let _ = run_control_owned(
            spec,
            &[rt.to_string(), "rm".into(), "-f".into(), spec.name.clone()],
            PROBE_TIMEOUT,
        );
    }
    // Build the image now (synchronous, correct ordering) when a Dockerfile
    // build was requested — the tag is `spec.image`, so the run below finds it.
    crate::sandbox_build::build_image(spec)?;
    use crate::progress::{SandboxPhase, emit};
    emit(SandboxPhase::ContainerCreate {
        backend: spec.backend.label(),
    });
    let mut argv: Vec<String> = oci_prefix(spec);
    argv.extend([
        "run".into(),
        "-d".into(),
        "--name".into(),
        spec.name.clone(),
    ]);
    argv.extend(oci_create_opts(spec));
    argv.push(effective_image(spec));
    argv.extend(["sleep".into(), "infinity".into()]);
    run_control_owned(spec, &argv, RUN_TIMEOUT);
    // Don't trust the exit code of `podman run -d`: on NixOS with broken
    // --userns keep-id, crun exits 0 but leaves the container in "created"
    // state. Verify it is actually running before declaring success.
    if container_status(spec).0 {
        emit(SandboxPhase::PhaseDone);
        return Ok(());
    }

    // Some rootless Podman/crun combinations (seen on NixOS) fail every
    // container started with `--userns keep-id` even though ordinary rootless
    // containers work. Retry without keep-id so an explicit rootless Podman
    // selection still produces a real container instead of forcing host use.
    if spec.backend == Backend::Podman {
        let _ = run_control_owned(
            spec,
            &[
                spec.backend.binary().to_string(),
                "rm".into(),
                "-f".into(),
                spec.name.clone(),
            ],
            PROBE_TIMEOUT,
        );
        let mut retry: Vec<String> = oci_prefix(spec);
        retry.extend([
            "run".into(),
            "-d".into(),
            "--name".into(),
            spec.name.clone(),
        ]);
        retry.extend(oci_create_opts_with_keep_id(spec, false));
        retry.push(effective_image(spec));
        retry.extend(["sleep".into(), "infinity".into()]);
        run_control_owned(spec, &retry, RUN_TIMEOUT);
        if container_status(spec).0 {
            msg::warn(
                "podman --userns keep-id failed; continuing with rootless podman default user namespace",
            );
            emit(SandboxPhase::PhaseDone);
            return Ok(());
        }
    }

    let err = format!("could not start {rt} container '{}'", spec.name);
    emit(SandboxPhase::PhaseFailed { err: err.clone() });
    anyhow::bail!(err)
}

/// Tear down the container for a worktree identified only by its local path.
/// Tries all OCI backends; silently ignores errors. Intended for background
/// cleanup when a worktree is closed and only its path is known (no cfg/loc).
pub fn teardown_by_path(worktree: &str) {
    let name = container_name(worktree);
    // Also remove the agent's separate container (when `agent_profile` differs
    // it runs in `thegn-{slug}-szagent`); `rm -f` of a non-existent name is a
    // harmless no-op.
    let agent = agent_container_name(&name);
    // Also remove the VPN sidecar (`thegn-{slug}-szvpn`) when one was started;
    // `rm -f` of a missing name is a harmless no-op. (Ephemeral node de-register
    // is the host's job via `thegn-svc::vpn::down` before this runs.)
    let vpn = vpn_sidecar_name(&name);
    let placement = Placement::Local;
    for b in [
        Backend::Podman,
        Backend::PodmanRootful,
        Backend::Docker,
        Backend::Smol,
        Backend::Apple,
    ] {
        if available(&placement, b) == RuntimeProbe::Present {
            let mut argv = backend_prefix(b);
            argv.extend([
                "rm".into(),
                "-f".into(),
                name.to_string(),
                agent.clone(),
                vpn.clone(),
            ]);
            let _ = run_control_t_owned(&placement, &argv, PROBE_TIMEOUT);
        }
    }
}

/// Remove a worktree's persistent container (OCI backends). Best-effort. Runs on
/// the worktree's host (local or remote, per its `GitLoc`).
pub fn teardown(cfg: &SandboxConfig, loc: &GitLoc, name: &str) {
    if !cfg.enabled {
        return;
    }
    let placement = placement_from_loc(cfg, loc);
    // Remove both the worktree container and the agent's separate container (the
    // latter only exists when `agent_profile` differs); `rm -f` of a missing
    // name is a harmless no-op.
    let agent = agent_container_name(name);
    let vpn = vpn_sidecar_name(name);
    // Try whichever OCI runtimes are available; the container only exists under one.
    for b in [
        Backend::Podman,
        Backend::PodmanRootful,
        Backend::Docker,
        Backend::Smol,
        Backend::Apple,
    ] {
        if available(&placement, b) == RuntimeProbe::Present {
            let mut argv = backend_prefix(b);
            argv.extend([
                "rm".into(),
                "-f".into(),
                name.to_string(),
                agent.clone(),
                vpn.clone(),
            ]);
            let _ = run_control_t_owned(&placement, &argv, PROBE_TIMEOUT);
        }
    }
}

/// Run the host-side `[sandbox] prepare` hooks for a worktree: each command via
/// `sh -lc` in the worktree dir, sequentially, on a background thread
/// (fire-and-forget — a failing hook is the user's script's concern). These run
/// on the HOST — with the writable Nix store, daemon, and full network — unlike
/// `init_script`, which runs *inside* the sandbox. Returns immediately; no-op
/// for an empty hook list. Subprocess seam (cov_ignore), exercised by smoke.
pub fn run_prepare(worktree: &std::path::Path, cmds: &[String]) {
    let cmds: Vec<String> = cmds
        .iter()
        .filter(|c| !c.trim().is_empty())
        .cloned()
        .collect();
    if cmds.is_empty() {
        return;
    }
    let wt = worktree.to_path_buf();
    std::thread::spawn(move || {
        for c in &cmds {
            // `detached`: null stdio + own group so a hook can't steal the tty.
            let _ = crate::util::detached("sh")
                .arg("-lc")
                .arg(c)
                .current_dir(&wt)
                .status();
        }
    });
}

/// The full argv to exec for an interactive pane running `inner` (a shell command
/// string, e.g. `${SHELL:-/bin/sh} -l` or `claude`). Wraps the backend invocation
/// in the transport (mosh/ssh) when remote.
pub fn enter_argv(spec: &SandboxSpec, inner: &str) -> Vec<String> {
    let script = wrap_script(spec, inner);
    // A compose-backed spec with a named service attaches through
    // `docker compose exec <service>` — no container-name guessing.
    let backend_argv = spec
        .compose_spec()
        .filter(|c| c.has_service())
        .and_then(|c| {
            let workdir = (spec.file_access != FileAccess::None)
                .then(|| spec.worktree.to_string_lossy().into_owned());
            crate::sandbox_compose::exec_argv(&spec.name, &c, workdir.as_deref(), &script, true)
        })
        .unwrap_or_else(|| backend_enter_argv(spec, &script));
    // Cap the pane's CPU on host-toolchain backends (no-op unless configured; see
    // [`crate::sandbox_cpucap`]). OCI/Systemd cap inline in their backend argv.
    let backend_argv = crate::sandbox_cpucap::wrap_pane_argv(spec, backend_argv);
    spec.placement.interactive_argv(&backend_argv)
}

/// Compose init-script + safe.directory + devenv into the `sh -lc` body that the
/// backend ultimately runs. The chosen program is `exec`'d so it owns the pane.
fn wrap_script(spec: &SandboxSpec, inner: &str) -> String {
    let mut s = String::new();
    if spec.backend.is_oci() {
        // Bind-mounted worktree is owned by a different uid under userns/root.
        s.push_str("git config --global --add safe.directory '*' >/dev/null 2>&1 || true\n");
    }
    // Unset blocked env keys (e.g. master API key when a scoped key replaces it).
    for key in &spec.env_block {
        s.push_str(&format!("unset {key}\n"));
    }
    // Inject per-agent env overrides (e.g. scoped virtual API key from the proxy).
    // Sort for determinism in tests.
    let mut overrides: Vec<(&String, &String)> = spec.env_overrides.iter().collect();
    overrides.sort_by_key(|(k, _)| k.as_str());
    for (key, val) in overrides {
        // Single-quote the value to be safe with special characters.
        let safe = val.replace('\'', "'\\''");
        s.push_str(&format!("export {key}='{safe}'\n"));
    }
    if let Some(init) = &spec.init_script {
        s.push_str(init);
        s.push('\n');
    }
    if spec.devenv {
        // Prefer the absolute path resolved at spec-build time so OCI containers
        // (which don't inherit the host PATH) can exec devenv without it being on
        // their default PATH.
        let devenv = spec.devenv_path.as_deref().unwrap_or("devenv");
        s.push_str(&format!("exec {devenv} shell -- {inner}"));
    } else if inner.contains("&&") || inner.contains(';') {
        // Compound expressions (e.g. a shell probe chain like
        // `command -v zsh && exec zsh -l; exec bash -l`) must NOT be
        // prefixed with `exec` — `exec` only accepts a single command.
        // The individual `exec` calls inside the chain handle process
        // replacement; running the expression directly is correct.
        s.push_str(inner);
    } else {
        s.push_str(&format!("exec {inner}"));
    }
    s
}

/// The backend-specific argv that runs `/bin/sh -lc <script>` in the sandbox.
fn backend_enter_argv(spec: &SandboxSpec, script: &str) -> Vec<String> {
    let wt = spec.worktree.to_string_lossy().into_owned();
    match spec.backend {
        Backend::Podman
        | Backend::PodmanRootful
        | Backend::Docker
        | Backend::Smol
        | Backend::Apple
        | Backend::Wsl => {
            let mut v = oci_prefix(spec);
            v.extend(["exec".into(), "-it".into()]);
            if spec.file_access != FileAccess::None {
                v.extend(["--workdir".into(), wt]);
            }
            v.extend([
                spec.name.clone(),
                "/bin/sh".into(),
                "-lc".into(),
                script.to_string(),
            ]);
            if spec.backend == Backend::Wsl {
                // Aspirational: shell out into WSL's distro to run podman there.
                v.insert(0, "wsl.exe".into());
                v.insert(1, "--".into());
            }
            v
        }
        Backend::WinAppContainer | Backend::WinJobObject => {
            // These native Windows backends run the standard command, optionally
            // wrapperized by internal logic if requested, but from the process builder
            // perspective they just run the script through the user shell in cwd.
            // When spawn_with_env runs it, we could intercept and wrap in a job object.
            // For argv generation, we just emit the plain shell command since the real
            // isolation happens in the OS process creation syscalls.
            crate::shellinv::run_argv(&util::shell(), script)
        }
        Backend::Bwrap => {
            let mut v = vec!["bwrap".to_string()];
            // Paths hardcoded into the bwrap argv — anything already covered here
            // must be skipped when processing spec.mounts to avoid duplicate /
            // conflicting bind mounts. bwrap cannot create sub-mount-points inside
            // a read-only bind (e.g. /etc/profiles/per-user/blake inside --ro-bind
            // /etc /etc) and returns "Unable to mount source on destination".
            let mut hardcoded_parents: Vec<&str> = Vec::new();
            if matches!(spec.file_access, FileAccess::All | FileAccess::Host) {
                v.extend(["--dev-bind".into(), "/".into(), "/".into()]);
                hardcoded_parents.push("/");
            } else {
                // Do not expose host / wholesale. Bind the runtime substrate read-only,
                // then add the explicit worktree/cache mounts below.
                for path in BWRAP_SUBSTRATE.iter().copied() {
                    if std::path::Path::new(path).exists() {
                        v.extend(["--ro-bind".into(), path.into(), path.into()]);
                        hardcoded_parents.push(path);
                    }
                }
                v.extend([
                    "--dev".into(),
                    "/dev".into(),
                    "--proc".into(),
                    "/proc".into(),
                    "--tmpfs".into(),
                    "/tmp".into(),
                ]);
            }
            if spec.file_access != FileAccess::None {
                v.extend(["--chdir".into(), wt]);
            }
            for m in &spec.mounts {
                // Skip mounts already covered by a hardcoded parent — bwrap
                // cannot create a mount-point inside a read-only bind.
                let covered = hardcoded_parents
                    .iter()
                    .any(|p| std::path::Path::new(&m.dest).starts_with(p) && m.dest != *p);
                if covered {
                    continue;
                }
                // Also skip exact duplicates of already-hardcoded paths.
                let duplicate = hardcoded_parents.iter().any(|p| m.dest == *p);
                if duplicate {
                    continue;
                }
                let flag = if m.ro { "--ro-bind" } else { "--bind" };
                v.extend([flag.into(), m.host.clone(), m.dest.clone()]);
            }
            v.extend(["--unshare-pid".into(), "--die-with-parent".into()]);
            if spec.network == Network::None {
                v.push("--unshare-net".into());
            }
            // Hardening (bwrap): the root is already assembled read-only from
            // the --ro-bind substrate above, and unprivileged bwrap sets
            // no_new_privs implicitly — so honor only explicit capability drops
            // here. bwrap has no process cap; `pids_limit` is enforced on the
            // OCI/systemd backends instead.
            for cap in &spec.drop_capabilities {
                v.extend(["--cap-drop".into(), cap.clone()]);
            }
            for cap in &spec.add_capabilities {
                v.extend(["--cap-add".into(), cap.clone()]);
            }
            for (k, val) in &spec.env {
                // Keep host-sourced passthrough values (tokens, API keys) off
                // the argv — `--setenv K V` is world-readable in /proc/*/cmdline.
                // Local bwrap inherits the launcher's process env (the pane
                // spawn path injects spec.env there), so pairs matching the
                // host env can simply be omitted. Synthetic pairs
                // (THEGN_SANDBOX, NIX_REMOTE) don't match and still ride
                // --setenv; remote-wrapped bwrap keeps it for everything, as
                // the argv is the only env carrier through ssh.
                if spec.placement.is_local() && std::env::var(k).ok().as_deref() == Some(val) {
                    continue;
                }
                v.extend(["--setenv".into(), k.clone(), val.clone()]);
            }
            v.extend([
                "--".into(),
                "/bin/sh".into(),
                "-lc".into(),
                script.to_string(),
            ]);
            v
        }
        Backend::Systemd => {
            let mut v = vec![
                "systemd-run".to_string(),
                "--user".into(),
                "--pty".into(),
                "--quiet".into(),
                "--collect".into(),
                format!("--working-directory={}", spec.worktree.display()),
                "-p".into(),
                "PrivateTmp=yes".into(),
            ];
            if spec.network == Network::None {
                v.extend(["-p".into(), "PrivateNetwork=yes".into()]);
            }
            // Aggregate slice + per-pane CPU/memory ceiling (inline systemd props).
            v.extend(crate::sandbox_cpucap::systemd_cap_args(&spec.limits));
            // Hardening (systemd unit properties). ProtectSystem=yes keeps /usr
            // & /boot read-only; ProtectHome=read-only closes the $HOME gap so a
            // sandboxed process can't `cd` out of the worktree and modify/delete
            // host files (the reported bwrap escape, same class here). The
            // read-write paths — worktree, git dir, build caches, and the narrow
            // $HOME carve-outs — are carved back from the non-ro `spec.mounts`;
            // PrivateTmp=yes already gives a writable /tmp.
            if spec.read_only_root {
                v.extend(["-p".into(), "ProtectSystem=yes".into()]);
                v.extend(["-p".into(), "ProtectHome=read-only".into()]);
                for m in &spec.mounts {
                    if !m.ro {
                        v.extend(["-p".into(), format!("ReadWritePaths={}", m.dest)]);
                    }
                }
                // $HOME is otherwise read-only; carve the same narrow writable
                // state dirs as bwrap so shell history/zoxide keep working. The
                // sealed agent drops ALL caps — mirror bwrap's keychain gating so
                // the sealed profile doesn't get a writable ~/.keychain.
                let sealed = spec
                    .drop_capabilities
                    .iter()
                    .any(|c| c.eq_ignore_ascii_case("ALL"));
                let carve_profile = if sealed {
                    SandboxProfile::Sealed
                } else {
                    SandboxProfile::Hardened
                };
                for cv in default_writable_carveouts(carve_profile) {
                    v.extend(["-p".into(), format!("ReadWritePaths={}", cv.dest)]);
                }
            }
            if spec.no_new_privileges {
                v.extend(["-p".into(), "NoNewPrivileges=yes".into()]);
            }
            if spec
                .drop_capabilities
                .iter()
                .any(|c| c.eq_ignore_ascii_case("ALL"))
            {
                v.extend(["-p".into(), "CapabilityBoundingSet=".into()]);
            }
            if let Some(p) = spec.pids_limit {
                v.extend(["-p".into(), format!("TasksMax={p}")]);
            }
            // systemd doesn't consume `spec.mounts`; translate the read-only
            // shared `.git/config` mount to a ReadOnlyPaths so it can't be
            // polluted here either. Match `/config` specifically — host-toolchain
            // and cache mounts never end in `/config`, so $HOME stays writable.
            for m in &spec.mounts {
                if m.ro && m.dest.ends_with("/config") {
                    v.extend(["-p".into(), format!("ReadOnlyPaths={}", m.dest)]);
                }
            }
            for (k, val) in &spec.env {
                v.extend(["--setenv".into(), format!("{k}={val}")]);
            }
            v.extend(["/bin/sh".into(), "-lc".into(), script.to_string()]);
            v
        }
        Backend::None => {
            // Bare shell (reached only for a remote worktree — local `none` runs
            // on the host via the caller). `spec.worktree` is only rewritten to a
            // real remote path for OCI backends (`retarget_if_remote_oci`); for a
            // bare remote shell it's still the LOCAL path, which doesn't exist on
            // the remote. Only `cd` when the target is known to be a real path on
            // the exec host: a local placement (host `none`), or a remote whose
            // worktree was retargeted (mounts point at it). Otherwise start in the
            // remote `$HOME` rather than ship a `cd <local-path>` that fails with
            // "cd: can't cd to …".
            let cd_ok = spec.placement.is_local() || spec.mounts.iter().any(|m| m.dest == wt);
            let body = if cd_ok {
                format!("cd {} && {script}", util::sh_quote(&wt))
            } else {
                script.to_string()
            };
            vec!["/bin/sh".into(), "-lc".into(), body]
        }
    }
}

/// OCI `run` options shared by the keep-alive container: mounts, network, env,
/// and uid mapping so bind-mounted files stay host-owned.
fn oci_create_opts(spec: &SandboxSpec) -> Vec<String> {
    let mut v = Vec::new();
    match spec.backend {
        Backend::Podman => v.extend(["--userns".into(), "keep-id".into()]),
        Backend::PodmanRootful => {
            if spec.placement.is_local()
                && let Some((uid, gid)) = local_uid_gid()
            {
                v.extend(["--user".into(), format!("{uid}:{gid}")]);
            }
        }
        Backend::Docker | Backend::Smol => {
            if spec.placement.is_local()
                && let Some((uid, gid)) = local_uid_gid()
            {
                v.extend(["--user".into(), format!("{uid}:{gid}")]);
            }
        }
        _ => {}
    }
    // When a VPN sidecar owns the netns (sidecar/proxy mode), the worktree
    // container joins it and its only egress is the tunnel. `--network
    // container:` is mutually exclusive with `--dns`/`-p`/other `--network`
    // flags (podman/docker reject them), so those are suppressed below.
    let vpn_join = vpn_sidecar_join(spec);
    if let Some(sidecar) = &vpn_join {
        v.extend(["--network".into(), format!("container:{sidecar}")]);
    } else {
        match spec.network {
            Network::Host => v.extend(["--network".into(), "host".into()]),
            Network::None => v.extend(["--network".into(), "none".into()]),
            Network::Nat => {}
        }
    }
    // `in_container` VPN mode runs the tunnel client inside this very container,
    // so it needs the tunnel capabilities here (this is the explicit, less-
    // isolated mode; sidecar mode keeps the worktree's caps untouched).
    if let Some(vpn) = &spec.vpn
        && vpn.mode == VpnMode::InContainer
    {
        v.extend([
            "--cap-add".into(),
            "NET_ADMIN".into(),
            "--device".into(),
            "/dev/net/tun".into(),
        ]);
    }
    // DNS-based domain filtering: start the proxy on first use and point the
    // container at it. Skip when network is None (DNS unreachable anyway), when
    // a VPN sidecar owns DNS (`--dns` is illegal on a container-netns join), or
    // when no filtering is configured.
    if spec.network != Network::None
        && vpn_join.is_none()
        && (!spec.network_allow.is_empty() || !spec.network_block.is_empty())
        && spec.placement.is_local()
    {
        let policy = crate::dns_filter::DnsPolicy {
            allow: spec.network_allow.clone(),
            block: spec.network_block.clone(),
            upstream: None,
        };
        if let Some(port) = crate::dns_filter::get_or_start(policy) {
            v.extend(["--dns".into(), format!("127.0.0.1:{port}")]);
        }
    }
    for m in &spec.mounts {
        // Never bind-mount the host's DNS/hosts files into an OCI container:
        // it has its own netns, so the runtime synthesizes a correct resolv.conf
        // (for NAT it rewrites loopback resolvers — systemd-resolved's 127.0.0.53,
        // a dnsmasq/Tailscale stub at 127.0.0.1 — to the NAT gateway, keeping
        // routable nameservers + search domains). Force-mounting the host file
        // (loopback-only on any systemd-resolved box) points DNS at the
        // container's own empty loopback → "Could not resolve host". bwrap/systemd
        // share the host netns and keep these mounts (loopback works there); this
        // also unshadows the `--dns` filter injection above.
        if !oci_emits_mount(m) {
            continue;
        }
        let suffix = if m.ro { ":ro" } else { "" };
        v.extend(["-v".into(), format!("{}:{}{suffix}", m.host, m.dest)]);
    }
    // When devenv lives in the Nix store, bind-mount /nix read-only so the
    // container can exec the resolved absolute path. Consistent with bwrap
    // which already does `--ro-bind /nix/store /nix/store`.
    if spec.devenv
        && let Some(p) = &spec.devenv_path
        && p.starts_with("/nix")
        && std::path::Path::new("/nix").exists()
    {
        v.extend(["-v".into(), "/nix:/nix:ro".into()]);
    }
    for (k, val) in &spec.env {
        v.extend(["-e".into(), format!("{k}={val}")]);
    }
    for (vol_name, dest) in &spec.volumes {
        v.extend(["-v".into(), format!("{}:{}", vol_name, dest)]);
    }

    if let Some(gpu) = &spec.gpu {
        if spec.backend == Backend::Docker || spec.backend == Backend::Smol {
            v.extend(["--gpus".into(), gpu.clone()]);
        } else if spec.backend == Backend::Podman {
            v.extend(["--device".into(), "nvidia.com/gpu=all".into()]);
        }
    }

    if let Some(c) = &spec.limits.cpu {
        v.extend(["--cpus".into(), c.clone()]);
    }
    if let Some(m) = &spec.limits.memory {
        v.extend(["--memory".into(), m.clone()]);
    }

    // Hardening knobs (resolved from the active SandboxProfile). Read-only root
    // needs writable tmpfs scratch for /tmp and /run so the shell and common
    // tools still work; the worktree + cache binds are already rw.
    if spec.read_only_root {
        v.extend([
            "--read-only".into(),
            "--tmpfs".into(),
            "/tmp".into(),
            "--tmpfs".into(),
            "/run".into(),
        ]);
    }
    for cap in &spec.drop_capabilities {
        v.extend(["--cap-drop".into(), cap.clone()]);
    }
    for cap in &spec.add_capabilities {
        v.extend(["--cap-add".into(), cap.clone()]);
    }
    if spec.no_new_privileges {
        v.extend(["--security-opt".into(), "no-new-privileges".into()]);
    }
    if let Some(p) = spec.pids_limit {
        v.extend(["--pids-limit".into(), p.to_string()]);
    }

    // Published ports must live on the netns owner. When a VPN sidecar owns the
    // netns, `-p` is illegal on the joining worktree container (it should be set
    // on the sidecar instead); warn and skip rather than fail the create.
    if vpn_join.is_none() {
        for p in &spec.ports {
            v.extend(["-p".into(), p.clone()]);
        }
    } else if !spec.ports.is_empty() {
        msg::warn(
            "sandbox: [sandbox] ports are ignored when a VPN sidecar owns the \
             network namespace; publish them on the sidecar instead.",
        );
    }
    v
}

/// When a VPN sidecar owns this worktree's network namespace (`sidecar`/`proxy`
/// mode), the worktree OCI container joins it via `--network container:<name>`
/// and MUST NOT also set `--dns`/`-p`/another `--network` (podman/docker reject
/// those on a container-netns join). Returns the sidecar name, or `None` when no
/// sidecar is in play (no VPN, or `in_container`/`netns` mode).
fn vpn_sidecar_join(spec: &SandboxSpec) -> Option<String> {
    let vpn = spec.vpn.as_ref()?;
    matches!(vpn.mode, VpnMode::Sidecar | VpnMode::Proxy).then(|| vpn_sidecar_name(&spec.name))
}

/// Like [`oci_create_opts`] but lets the caller suppress Podman's
/// `--userns keep-id` flag for the rootless-fallback retry path.
fn oci_create_opts_with_keep_id(spec: &SandboxSpec, keep_id: bool) -> Vec<String> {
    let mut v = Vec::new();
    match spec.backend {
        Backend::Podman if keep_id => {
            v.extend(["--userns".into(), "keep-id".into()]);
        }
        Backend::Podman => {}
        Backend::PodmanRootful => {
            if spec.placement.is_local()
                && let Some((uid, gid)) = local_uid_gid()
            {
                v.extend(["--user".into(), format!("{uid}:{gid}")]);
            }
        }
        Backend::Docker | Backend::Smol => {
            if spec.placement.is_local()
                && let Some((uid, gid)) = local_uid_gid()
            {
                v.extend(["--user".into(), format!("{uid}:{gid}")]);
            }
        }
        _ => {}
    }
    // All other opts (network, mounts, env, volumes, gpu, limits, ports) are
    // identical to oci_create_opts — delegate by temporarily re-routing:
    // build via oci_create_opts and strip the userns flag if present.
    let mut full = oci_create_opts(spec);
    if spec.backend == Backend::Podman && !keep_id {
        // Drop "--userns" and "keep-id" (two consecutive entries).
        let mut out = Vec::with_capacity(full.len());
        let mut skip = false;
        for item in full.drain(..) {
            if item == "--userns" {
                skip = true;
                continue;
            }
            if skip && item == "keep-id" {
                skip = false;
                continue;
            }
            skip = false;
            out.push(item);
        }
        out
    } else {
        full
    }
}

pub(crate) fn backend_prefix(backend: Backend) -> Vec<String> {
    match backend {
        Backend::PodmanRootful => vec!["sudo".into(), "-n".into(), "podman".into()],
        _ => vec![backend.binary().into()],
    }
}

/// The OCI runtime prefix for a *resolved spec*, including the remote-daemon
/// connection flag when `[sandbox] oci_host` is set (drives a remote daemon
/// instead of SSH-wrapping the whole argv). podman takes `--url <ssh://…>` (or
/// `--connection <name>` for a configured connection); docker takes `-H <host>`.
/// Falls back to the plain [`backend_prefix`] for the local daemon or a non-OCI
/// backend. Used by every container lifecycle/exec call so create, inspect, exec
/// and teardown all target the same daemon.
pub(crate) fn oci_prefix(spec: &SandboxSpec) -> Vec<String> {
    let mut v = backend_prefix(spec.backend);
    let Some(host) = spec
        .oci_host
        .as_deref()
        .map(str::trim)
        .filter(|h| !h.is_empty())
    else {
        return v;
    };
    if !spec.backend.is_oci() {
        return v;
    }
    // Insert the global connection flag right after the binary (before the
    // subcommand). For rootful podman the binary is at index 2 (`sudo -n podman`).
    let bin_idx = if spec.backend == Backend::PodmanRootful {
        2
    } else {
        0
    };
    let flags: Vec<String> = match spec.backend {
        Backend::Docker => vec!["-H".into(), host.to_string()],
        // podman: a URL (scheme://) is `--url`; a bare token is a named `--connection`.
        _ if host.contains("://") => vec!["--url".into(), host.to_string()],
        _ => vec!["--connection".into(), host.to_string()],
    };
    for (i, f) in flags.into_iter().enumerate() {
        v.insert(bin_idx + 1 + i, f);
    }
    v
}

/// The argv prefix to invoke the container CLI for an OCI `backend`
/// (`["podman"]`, `["sudo", "-n", "podman"]`, `["docker"]`, …), or `None` for a
/// non-OCI backend. Lets the host drive a VPN sidecar via the *same* runtime as
/// the worktree container (so `--network container:` shares a user namespace).
pub fn oci_runtime_prefix(backend: Backend) -> Option<Vec<String>> {
    backend.is_oci().then(|| backend_prefix(backend))
}

pub(crate) fn run_local_output(prefix: &[String], args: &[&str]) -> Option<String> {
    let (cmd, rest) = prefix.split_first()?;
    let mut c = Command::new(cmd);
    c.args(rest).args(args);
    let out = c.output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).to_string())
}

/// Run a control-plane command (locally, or on the remote over ssh). Returns
/// whether it succeeded.
pub(crate) fn run_control_owned(
    spec: &SandboxSpec,
    argv: &[String],
    timeout: Duration,
) -> Option<bool> {
    run_control_t_owned(&spec.placement, argv, timeout)
}

fn run_control_t_owned(placement: &Placement, argv: &[String], timeout: Duration) -> Option<bool> {
    let argv = placement.control_argv(argv);
    status_with_timeout(&argv, timeout)
}

/// Like [`run_control_t_owned`] but also captures stdout. Wraps argv through the
/// placement's control primitive (ssh batch / kubectl exec / provider).
fn output_control_owned(
    spec: &SandboxSpec,
    argv: &[String],
    timeout: Duration,
) -> Option<(bool, String)> {
    let full: Vec<String> = spec.placement.control_argv(argv);
    output_with_timeout(&full, timeout)
}

/// Local uid/gid via `id` (no libc dep). None if `id` is unavailable.
fn local_uid_gid() -> Option<(u32, u32)> {
    let uid = Command::new("id").arg("-u").output().ok()?;
    let gid = Command::new("id").arg("-g").output().ok()?;
    let u = String::from_utf8_lossy(&uid.stdout).trim().parse().ok()?;
    let g = String::from_utf8_lossy(&gid.stdout).trim().parse().ok()?;
    Some((u, g))
}

/// Runtime substrate the bwrap backend hardcodes into its argv (read-only).
/// Anything covered here must be skipped when emitting `spec.mounts`, and
/// counts as "already reachable in-sandbox" for identity-key resolution.
pub const BWRAP_SUBSTRATE: &[&str] = &[
    "/nix/store",
    "/run/current-system",
    "/bin",
    "/usr",
    "/lib",
    "/lib64",
    "/etc",
];

// SSH credential plumbing (flattened-config materialization, identity-key
// mounts) lives in `crate::ssh_creds`; re-exported here for existing callers.
pub use crate::ssh_creds::prepare_ssh_config;

#[derive(Debug, Default, Clone)]
pub struct SandboxStats {
    pub cpu: String,
    pub mem: String,
}

pub fn stats(spec: &SandboxSpec) -> Option<SandboxStats> {
    if !spec.backend.is_oci() {
        return None;
    }
    let rt = spec.backend.binary();
    // format: CPUPerc|MemUsage
    let argv = [
        rt,
        "stats",
        "--no-stream",
        "--format",
        "{{.CPUPerc}}|{{.MemUsage}}",
        &spec.name,
    ];

    let out = std::process::Command::new(argv[0])
        .args(&argv[1..])
        .output()
        .ok()?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    parse_sandbox_stats(stdout.trim())
}

fn parse_sandbox_stats(output: &str) -> Option<SandboxStats> {
    let parts: Vec<&str> = output.split('|').collect();
    if parts.len() != 2 {
        return None;
    }
    let mem = parts[1]
        .split('/')
        .next()
        .unwrap_or(parts[1])
        .trim()
        .to_string();
    Some(SandboxStats {
        cpu: parts[0].trim().to_string(),
        mem,
    })
}

pub fn identify_orphans(active_worktrees: &[String], containers: &[String]) -> Vec<String> {
    // A live worktree owns EVERY container that reduces to its slug: the plain
    // `thegn-{slug}`, the profile variant `thegn-{profile}-{slug}`
    // (`container_name_with_profile`), and the `-szagent` / `-szvpn` companions of
    // either. Reconcile by reverse-mapping each candidate back to a worktree slug
    // rather than allow-listing exact names — the old allow-list only knew the
    // plain + `-szagent` forms, so a session launched with a non-default profile
    // or a VPN sidecar was misread as an orphan and force-removed while live.
    // Reaping is fail-closed: any container that maps to an active worktree by any
    // of these forms is kept.
    let active_slugs: Vec<String> = active_worktrees
        .iter()
        .map(|w| util::slugify(w))
        .collect();

    containers
        .iter()
        .filter(|c| c.starts_with(CONTAINER_PREFIX))
        .filter(|c| {
            // Strip the companion suffixes to get the worktree/profile container
            // base (`thegn-{slug}` or `thegn-{profile}-{slug}`).
            let base = strip_vpn_suffix(strip_agent_suffix(c));
            let Some(rest) = base.strip_prefix(CONTAINER_PREFIX) else {
                return true;
            };
            // Orphan unless `rest` is an active slug (plain form) or ends with
            // `-{slug}` (profile-prefixed form) for some active worktree.
            !active_slugs
                .iter()
                .any(|s| rest == s || rest.ends_with(&format!("-{s}")))
        })
        .cloned()
        .collect()
}

/// Remove orphaned thegn containers (containers whose worktree no longer
/// exists in the DB). Returns the names of containers that were removed.
pub fn run_gc(db_worktrees: &[String]) -> Vec<String> {
    let mut removed = Vec::new();
    for backend in [Backend::Podman, Backend::Docker, Backend::Smol] {
        if !crate::util::have(backend.binary()) {
            continue;
        }

        let Ok(out) = std::process::Command::new(backend.binary())
            .args(["ps", "-a", "--format", "{{.Names}}"])
            .output()
        else {
            continue;
        };

        let stdout = String::from_utf8_lossy(&out.stdout);
        let containers: Vec<String> = stdout.lines().map(|s| s.trim().to_string()).collect();

        for orphan in identify_orphans(db_worktrees, &containers) {
            let _ = std::process::Command::new(backend.binary())
                .args(["rm", "-f", &orphan])
                .output();
            removed.push(orphan);
        }
    }
    removed
}

#[cfg(test)]
#[path = "sandbox_tests.rs"]
mod tests;
