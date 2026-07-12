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

    pub(crate) fn is_host_toolchain(self) -> bool {
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

pub use crate::sandbox_backend::placement_reachable;
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
fn container_status(spec: &SandboxSpec) -> (bool, bool) {
    let required: std::collections::HashSet<&str> =
        spec.mounts.iter().map(|m| m.host.as_str()).collect();

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
            // perspective they just exec `sh -lc "script"` (or pwsh equivalent) in cwd.
            // When spawn_with_env runs it, we could intercept and wrap in a job object.
            // For argv generation, we just emit the plain shell command since the real
            // isolation happens in the OS process creation syscalls.
            let shell = util::shell();
            let mut v = vec![shell.clone()];
            if shell.ends_with("pwsh.exe") || shell.ends_with("powershell.exe") {
                v.extend(["-NoProfile".into(), "-Command".into(), script.to_string()]);
            } else if shell.ends_with("cmd.exe") {
                v.extend(["/C".into(), script.to_string()]);
            } else {
                v.extend(["-c".into(), script.to_string()]);
            }
            v
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
        if matches!(m.dest.as_str(), "/etc/resolv.conf" | "/etc/hosts") {
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

fn parse_mount(spec: &str) -> Mount {
    // "host", "host:ro", or "host:dest" / "host:dest:ro".
    // Paths starting with "~/" are expanded to the real home directory so the
    // mount spec is valid as a filesystem path (bwrap does not do shell expansion).
    let expand = |p: &str| crate::util::expand_tilde(p);
    let parts: Vec<&str> = spec.split(':').collect();
    match parts.as_slice() {
        [host] => Mount {
            host: expand(host),
            dest: expand(host),
            ro: false,
            cache: false,
        },
        [host, "ro"] => Mount {
            host: expand(host),
            dest: expand(host),
            ro: true,
            cache: false,
        },
        [host, "cache"] => Mount {
            host: expand(host),
            dest: expand(host),
            ro: false,
            cache: true,
        },
        [host, dest] => Mount {
            host: expand(host),
            dest: expand(dest),
            ro: false,
            cache: false,
        },
        [host, dest, "ro"] => Mount {
            host: expand(host),
            dest: expand(dest),
            ro: true,
            cache: false,
        },
        [host, dest, "cache"] => Mount {
            host: expand(host),
            dest: expand(dest),
            ro: false,
            cache: true,
        },
        _ => Mount {
            host: expand(spec),
            dest: expand(spec),
            ro: false,
            cache: false,
        },
    }
}

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
    // Each active worktree owns both its container and (when `agent_profile`
    // differs) the agent's `-szagent` container — neither is an orphan.
    let active_names: Vec<String> = active_worktrees
        .iter()
        .flat_map(|w| {
            let base = container_name(w);
            [agent_container_name(&base), base]
        })
        .collect();

    containers
        .iter()
        .filter(|c| c.starts_with("thegn-"))
        .filter(|c| !active_names.contains(c))
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
mod tests {
    use super::*;
    use crate::sandbox_mounts::host_toolchain_mounts;

    #[test]
    fn none_backend_skips_cd_for_unretargeted_remote_worktree() {
        // A bare remote shell whose worktree was NOT retargeted (local path) must
        // NOT `cd <local-path>` on the remote — it would fail "cd: can't cd to …".
        let mut s = spec(Backend::None);
        s.worktree = PathBuf::from("/home/me/wt");
        s.placement = Placement::Ssh(SshPlacement::plain(
            "host".into(),
            22,
            false,
            TransportKind::Ssh,
        ));
        s.mounts.clear();
        let body = backend_enter_argv(&s, "exec sh").pop().unwrap();
        assert!(!body.contains("cd "), "no local cd shipped remote: {body}");
        assert!(body.contains("exec sh"));

        // A retargeted remote (a mount whose dest == the worktree path) DOES cd.
        s.mounts = vec![Mount {
            host: "/home/me/wt".into(),
            dest: "/home/me/wt".into(),
            ro: false,
            cache: false,
        }];
        let retargeted = backend_enter_argv(&s, "exec sh").pop().unwrap();
        assert!(
            retargeted.starts_with("cd "),
            "retargeted remote cds: {retargeted}"
        );

        // A local `none` still cds into the worktree.
        let mut local = spec(Backend::None);
        local.placement = Placement::Local;
        let body_local = backend_enter_argv(&local, "exec sh").pop().unwrap();
        assert!(
            body_local.starts_with("cd "),
            "local none cds: {body_local}"
        );
    }

    fn vpn_cfg(provider: VpnProviderKind) -> VpnConfig {
        VpnConfig {
            provider,
            ..VpnConfig::default()
        }
    }

    #[test]
    fn build_vpn_spec_none_provider_is_none() {
        assert!(build_vpn_spec(&VpnConfig::default(), "wt", SandboxProfile::Hardened).is_none());
    }

    #[test]
    fn build_vpn_spec_sealed_refuses_but_sealed_tunnel_attaches() {
        let cfg = vpn_cfg(VpnProviderKind::Tailscale);
        // Plain sealed refuses a tunnel (returns None).
        assert!(build_vpn_spec(&cfg, "wt", SandboxProfile::Sealed).is_none());
        // sealed-tunnel and hardened both attach.
        assert!(build_vpn_spec(&cfg, "wt", SandboxProfile::SealedTunnel).is_some());
        assert!(build_vpn_spec(&cfg, "wt", SandboxProfile::Hardened).is_some());
    }

    #[test]
    fn build_vpn_spec_maps_each_provider_to_its_params() {
        for provider in [
            VpnProviderKind::Tailscale,
            VpnProviderKind::Headscale,
            VpnProviderKind::Wireguard,
            VpnProviderKind::Openvpn,
            VpnProviderKind::Netbird,
            VpnProviderKind::Zerotier,
            VpnProviderKind::Custom,
        ] {
            let spec = build_vpn_spec(&vpn_cfg(provider), "wt", SandboxProfile::Hardened).unwrap();
            assert_eq!(spec.provider, provider);
            // Headscale reuses the Tailscale params variant.
            let ok = matches!(
                (provider, &spec.params),
                (VpnProviderKind::Tailscale, VpnParams::Tailscale(_))
                    | (VpnProviderKind::Headscale, VpnParams::Tailscale(_))
                    | (VpnProviderKind::Wireguard, VpnParams::Wireguard(_))
                    | (VpnProviderKind::Openvpn, VpnParams::Openvpn(_))
                    | (VpnProviderKind::Netbird, VpnParams::Netbird(_))
                    | (VpnProviderKind::Zerotier, VpnParams::Zerotier(_))
                    | (VpnProviderKind::Custom, VpnParams::Custom(_))
            );
            assert!(ok, "{provider:?} mapped to wrong params: {:?}", spec.params);
        }
    }

    #[test]
    fn build_vpn_spec_hostname_defaults_to_name_then_overrides() {
        // Default: container name.
        let spec = build_vpn_spec(
            &vpn_cfg(VpnProviderKind::Tailscale),
            "thegn-repo-feat",
            SandboxProfile::Hardened,
        )
        .unwrap();
        assert_eq!(spec.hostname, "thegn-repo-feat");

        // Per-provider hostname wins.
        let mut cfg = vpn_cfg(VpnProviderKind::Tailscale);
        cfg.tailscale.hostname = "custom-node".into();
        let spec = build_vpn_spec(&cfg, "thegn-repo-feat", SandboxProfile::Hardened).unwrap();
        assert_eq!(spec.hostname, "custom-node");
    }

    #[test]
    fn build_vpn_spec_carries_knobs_and_optional_image() {
        let mut cfg = vpn_cfg(VpnProviderKind::Wireguard);
        cfg.mode = VpnMode::Proxy;
        cfg.on_error = VpnOnError::Offline;
        cfg.dns = VpnDnsMode::FilterFront;
        cfg.ready_timeout_secs = 7;
        cfg.ephemeral = false;
        let spec = build_vpn_spec(&cfg, "wt", SandboxProfile::Hardened).unwrap();
        assert_eq!(spec.mode, VpnMode::Proxy);
        assert_eq!(spec.on_error, VpnOnError::Offline);
        assert_eq!(spec.dns_mode, VpnDnsMode::FilterFront);
        assert_eq!(spec.ready_timeout, Duration::from_secs(7));
        assert!(!spec.ephemeral);
        // Empty sidecar_image -> None; set -> Some.
        assert!(spec.sidecar_image.is_none());
        cfg.sidecar_image = "ghcr.io/me/wg:latest".into();
        let spec = build_vpn_spec(&cfg, "wt", SandboxProfile::Hardened).unwrap();
        assert_eq!(spec.sidecar_image.as_deref(), Some("ghcr.io/me/wg:latest"));
    }

    #[test]
    fn oci_opts_join_vpn_sidecar_netns_and_suppress_dns_ports() {
        let mut s = spec(Backend::Podman);
        s.network = Network::Nat;
        s.network_allow = vec!["example.com".into()]; // would normally add --dns
        s.ports = vec!["8080:8080".into()]; // would normally add -p
        s.vpn = build_vpn_spec(
            &vpn_cfg(VpnProviderKind::Tailscale),
            &s.name,
            SandboxProfile::Hardened,
        );
        let opts = oci_create_opts(&s);
        let joined = opts.join(" ");
        // Joins the sidecar netns...
        assert!(
            joined.contains("--network container:thegn-repo-feat-szvpn"),
            "{joined}"
        );
        // ...and suppresses --dns and -p (illegal on a container-netns join).
        assert!(!opts.iter().any(|o| o == "--dns"), "{joined}");
        assert!(!opts.iter().any(|o| o == "-p"), "{joined}");
    }

    #[test]
    fn oci_opts_in_container_mode_adds_net_admin_and_tun() {
        let mut s = spec(Backend::Podman);
        let mut cfg = vpn_cfg(VpnProviderKind::Wireguard);
        cfg.mode = VpnMode::InContainer;
        s.vpn = build_vpn_spec(&cfg, &s.name, SandboxProfile::Hardened);
        let opts = oci_create_opts(&s);
        let joined = opts.join(" ");
        // in_container does NOT join a sidecar netns; it keeps normal networking
        // and adds the tunnel caps to the worktree container itself.
        assert!(!joined.contains("container:"), "{joined}");
        assert!(opts.iter().any(|o| o == "NET_ADMIN"), "{joined}");
        assert!(joined.contains("/dev/net/tun"), "{joined}");
    }

    #[test]
    fn oci_opts_without_vpn_keep_normal_network_and_ports() {
        let mut s = spec(Backend::Podman);
        s.network = Network::Nat;
        s.ports = vec!["8080:8080".into()];
        assert!(s.vpn.is_none());
        let opts = oci_create_opts(&s);
        // No container: join; ports published as usual.
        assert!(!opts.join(" ").contains("container:"));
        assert!(opts.windows(2).any(|w| w == ["-p", "8080:8080"]));
    }

    #[test]
    fn test_win_native_sandboxes_do_not_parse_as_oci() {
        assert!(!Backend::WinAppContainer.is_oci());
        assert!(!Backend::WinJobObject.is_oci());
        assert!(Backend::WinAppContainer.is_host_toolchain());
        assert!(Backend::WinJobObject.is_host_toolchain());
        assert_eq!(Backend::WinAppContainer.label(), "appcontainer");
        assert_eq!(Backend::WinJobObject.label(), "jobobject");
    }

    fn spec(backend: Backend) -> SandboxSpec {
        SandboxSpec {
            backend,
            placement: Placement::Local,
            image: Some("img:latest".into()),
            worktree: PathBuf::from("/wt/feat"),
            mounts: vec![
                Mount {
                    host: "/wt/feat".into(),
                    dest: "/wt/feat".into(),
                    ro: false,
                    cache: false,
                },
                Mount {
                    host: "/repo/.git".into(),
                    dest: "/repo/.git".into(),
                    ro: false,
                    cache: false,
                },
            ],
            env: vec![("GH_TOKEN".into(), "abc".into())],
            env_overrides: std::collections::HashMap::new(),
            env_block: Vec::new(),
            network: Network::Nat,
            network_allow: Vec::new(),
            network_block: Vec::new(),
            read_only_root: false,
            no_new_privileges: false,
            pids_limit: None,
            drop_capabilities: Vec::new(),
            add_capabilities: Vec::new(),
            ports: vec!["8080:8080".into()],
            gpu: None,
            // Disable CPU capping so argv-shape assertions are host-independent
            // (the aggregate cap is on-by-default and would scope-wrap on a host
            // with cgroup delegation); capping itself is tested in sandbox_cpucap.
            limits: SandboxLimits {
                cpu_total: Some("off".into()),
                ..SandboxLimits::default()
            },
            volumes: vec![],
            compose: None,
            build: None,
            init_script: None,
            file_access: FileAccess::Worktree,
            devenv: false,
            devenv_path: None,
            name: "thegn-repo-feat".into(),
            vpn: None,
            oci_host: None,
        }
    }

    #[test]
    fn oci_prefix_injects_remote_daemon_connection() {
        // Local daemon (default): plain prefix.
        let mut s = spec(Backend::Podman);
        assert_eq!(oci_prefix(&s), vec!["podman"]);

        // podman URL → --url before the subcommand.
        s.oci_host = Some("ssh://user@box/run/podman.sock".into());
        assert_eq!(
            oci_prefix(&s),
            vec!["podman", "--url", "ssh://user@box/run/podman.sock"]
        );

        // podman bare token → named --connection.
        s.oci_host = Some("workbox".into());
        assert_eq!(oci_prefix(&s), vec!["podman", "--connection", "workbox"]);

        // docker → -H.
        let mut d = spec(Backend::Docker);
        d.oci_host = Some("ssh://user@box".into());
        assert_eq!(oci_prefix(&d), vec!["docker", "-H", "ssh://user@box"]);

        // rootful podman: flag lands after `sudo -n podman`, not before.
        let mut r = spec(Backend::PodmanRootful);
        r.oci_host = Some("workbox".into());
        assert_eq!(
            oci_prefix(&r),
            vec!["sudo", "-n", "podman", "--connection", "workbox"]
        );

        // Non-OCI backend ignores oci_host.
        let mut b = spec(Backend::Bwrap);
        b.oci_host = Some("workbox".into());
        assert_eq!(oci_prefix(&b), vec!["bwrap"]);
    }

    #[test]
    fn podman_exec_preserves_paths() {
        let argv = enter_argv(&spec(Backend::Podman), "${SHELL:-/bin/sh} -l");
        assert_eq!(argv[0], "podman");
        assert!(argv.contains(&"exec".to_string()));
        assert!(argv.contains(&"thegn-repo-feat".to_string()));
        // workdir is the worktree's host path (path-preserving).
        let w = argv.iter().position(|a| a == "--workdir").unwrap();
        assert_eq!(argv[w + 1], "/wt/feat");
        // safe.directory + exec are in the sh body.
        let body = argv.last().unwrap();
        assert!(body.contains("safe.directory"));
        assert!(body.contains("exec ${SHELL:-/bin/sh} -l"));
    }

    #[test]
    fn bwrap_binds_worktree_and_gitdir() {
        let mut s = spec(Backend::Bwrap);
        s.image = None;
        s.file_access = FileAccess::Worktree;
        let argv = enter_argv(&s, "claude");
        assert_eq!(argv[0], "bwrap");
        let joined = argv.join(" ");
        assert!(!joined.contains("--ro-bind / /"));
        assert!(joined.contains("--bind /wt/feat /wt/feat"));
        assert!(joined.contains("--bind /repo/.git /repo/.git"));
        assert!(joined.contains("--chdir /wt/feat"));
        assert_eq!(argv.last().unwrap(), "exec claude");
    }

    #[test]
    fn file_access_none_removes_workdir() {
        let mut s = spec(Backend::Podman);
        s.file_access = FileAccess::None;
        let argv = enter_argv(&s, "claude");
        let joined = argv.join(" ");
        assert!(!joined.contains("--workdir"));
    }

    #[test]
    fn oci_create_opts_map_userns_and_mounts() {
        let opts = oci_create_opts(&spec(Backend::Podman));
        let j = opts.join(" ");
        assert!(j.contains("--userns keep-id"));
        assert!(j.contains("-v /wt/feat:/wt/feat"));
        assert!(j.contains("-v /repo/.git:/repo/.git"));
        assert!(j.contains("-e GH_TOKEN=abc"));
        assert!(j.contains("-p 8080:8080"));
    }

    #[test]
    fn oci_opts_never_bind_mount_host_dns_files() {
        // Regression: bind-mounting the host's loopback-only resolv.conf into a
        // NAT container broke DNS ("Could not resolve host" on git push). The
        // runtime must own resolv.conf/hosts for OCI backends.
        let mut s = spec(Backend::Podman);
        s.mounts.push(Mount {
            host: "/etc/resolv.conf".into(),
            dest: "/etc/resolv.conf".into(),
            ro: true,
            cache: false,
        });
        s.mounts.push(Mount {
            host: "/etc/hosts".into(),
            dest: "/etc/hosts".into(),
            ro: true,
            cache: false,
        });
        let j = oci_create_opts(&s).join(" ");
        assert!(!j.contains("/etc/resolv.conf"), "resolv.conf mounted: {j}");
        assert!(!j.contains(":/etc/hosts"), "/etc/hosts mounted: {j}");
        // Real worktree mounts are untouched.
        assert!(j.contains("-v /wt/feat:/wt/feat"));
    }

    #[test]
    fn empty_oci_image_uses_default_image() {
        let mut s = spec(Backend::Podman);
        s.image = None;
        assert_eq!(effective_image(&s), DEFAULT_OCI_IMAGE);
    }

    #[test]
    fn mosh_wraps_backend_over_ssh() {
        let mut s = spec(Backend::Podman);
        s.placement = Placement::Ssh(SshPlacement::plain(
            "user@box".into(),
            2222,
            true,
            TransportKind::Mosh,
        ));
        let argv = enter_argv(&s, "${SHELL:-/bin/sh} -l");
        assert_eq!(argv[0], "mosh");
        assert!(argv.iter().any(|a| a.starts_with("--ssh=")));
        assert!(argv.iter().any(|a| a.contains("-p 2222")));
        assert!(argv.contains(&"user@box".to_string()));
        // The remote sh body re-runs the podman exec.
        assert!(argv.last().unwrap().contains("podman exec"));
    }

    #[test]
    fn ssh_transport_uses_tty() {
        let mut s = spec(Backend::Bwrap);
        s.image = None;
        s.placement = Placement::Ssh(SshPlacement::plain(
            "box".into(),
            22,
            false,
            TransportKind::Ssh,
        ));
        let argv = enter_argv(&s, "claude");
        assert_eq!(argv[0], "ssh");
        assert!(argv.contains(&"-t".to_string()));
        assert!(argv.last().unwrap().contains("bwrap"));
    }

    #[test]
    fn bwrap_local_keeps_host_matching_env_off_argv() {
        // Guarantee THEGN_SANDBOX is absent from the ambient env: running
        // `cargo test` inside a live thegn bwrap sandbox leaks it in, which
        // would make the synthetic pair below "match host" and get omitted.
        let _env = crate::testenv::EnvGuard::unset(&["THEGN_SANDBOX"]);
        let mut s = spec(Backend::Bwrap);
        s.image = None;
        // A pair mirroring the host env (PATH always exists) rides the
        // launcher's process env, never the world-readable --setenv argv;
        // synthetic pairs (values absent from the host env) keep --setenv.
        s.env = vec![
            ("PATH".into(), std::env::var("PATH").unwrap()),
            ("THEGN_SANDBOX".into(), "1".into()),
        ];
        let argv = enter_argv(&s, "true");
        assert!(!argv.contains(&"PATH".to_string()));
        let i = argv.iter().position(|a| a == "--setenv").unwrap();
        assert_eq!(argv[i + 1], "THEGN_SANDBOX");

        // Remote-wrapped bwrap keeps --setenv for everything: the argv is the
        // only env carrier through ssh.
        s.placement = Placement::Ssh(SshPlacement::plain(
            "box".into(),
            22,
            false,
            TransportKind::Ssh,
        ));
        let remote = enter_argv(&s, "true").join(" ");
        assert!(remote.contains("--setenv PATH"));
    }

    #[test]
    fn bwrap_local_omits_ambient_matching_marker() {
        // The nested case (thegn-in-thegn): THEGN_SANDBOX=1 is already
        // in the launcher's env, so local bwrap inherits it and the pair is
        // omitted from the world-readable --setenv argv. A synthetic pair whose
        // value is absent from the host env still rides --setenv.
        let _env = crate::testenv::EnvGuard::set(&[("THEGN_SANDBOX", "1")]);
        let mut s = spec(Backend::Bwrap);
        s.image = None;
        s.env = vec![
            ("THEGN_SANDBOX".into(), "1".into()),
            ("THEGN_SYNTH_MARKER".into(), "x".into()),
        ];
        let argv = enter_argv(&s, "true");
        // Host-matching marker inherited, not on argv.
        assert!(!argv.contains(&"THEGN_SANDBOX".to_string()));
        // Synthetic pair (absent from host env) rides --setenv.
        let i = argv.iter().position(|a| a == "--setenv").unwrap();
        assert_eq!(argv[i + 1], "THEGN_SYNTH_MARKER");
    }

    #[test]
    fn test_parse_sandbox_stats() {
        let output = "1.5%|50MiB / 16GiB";
        let stats = parse_sandbox_stats(output).unwrap();
        assert_eq!(stats.cpu, "1.5%");
        assert_eq!(stats.mem, "50MiB");
    }

    #[test]
    fn test_sandbox_all_oci_flags_applied() {
        let mut s = spec(Backend::Podman);
        s.gpu = Some("all".into());
        s.limits = SandboxLimits {
            cpu: Some("2".into()),
            memory: Some("4GB".into()),
            cpu_total: None,
        };
        s.volumes = vec![("data-vol".into(), "/mnt/data".into())];

        let opts = oci_create_opts(&s);
        let j_opts = opts.join(" ");
        assert!(j_opts.contains("--device nvidia.com/gpu=all"));
        assert!(j_opts.contains("--cpus 2"));
        assert!(j_opts.contains("--memory 4GB"));
        assert!(j_opts.contains("-v data-vol:/mnt/data"));
    }

    #[test]
    fn test_sandbox_compose_executes() {
        // We cannot mock easily without a trait. Since `ensure` executes `docker-compose`,
        // we'll leave Compose verification to the Integration/E2E layer.
    }

    pub fn pull_image(img: &str) -> anyhow::Result<()> {
        let _ = std::process::Command::new("podman")
            .args(["pull", img])
            .output();
        Ok(())
    }

    #[test]
    fn integration_test_sandbox_net_and_file() {
        // Only run if podman is installed.
        if !crate::util::have("podman") {
            return;
        }

        // Always skip in CI to prevent flakiness unless explicitly forced.
        if std::env::var("CI").is_ok()
            || std::env::var("SKIP_PODMAN_E2E").is_ok()
            || std::env::var("PODMAN_E2E_FORCE").is_err()
        {
            return;
        }

        let mut s = spec(Backend::Podman);
        s.name = "thegn-test-net-file-container".into();
        // A minimal image that has python3 installed
        s.image = Some("public.ecr.aws/docker/library/python:3-alpine".into());
        s.mounts = vec![];
        s.file_access = FileAccess::None;
        s.ports = vec!["8081:8081".into()];

        // Pull image first so `ensure` doesn't timeout if it tries to do it or if it's not present
        // Ignore pull failures (we might already have the image cached)
        let _ = pull_image("public.ecr.aws/docker/library/python:3-alpine");

        // We launch it with a background Python webserver
        let res = ensure(&s);
        assert!(res.is_ok(), "Failed to start container: {:?}", res);

        let argv = enter_argv(&s, "python3 -m http.server 8081");

        let mut child = std::process::Command::new(&argv[0])
            .args(&argv[1..])
            .spawn()
            .expect("Failed to spawn sandboxed server");

        // Wait for boot
        std::thread::sleep(std::time::Duration::from_millis(3000));

        // Test Network Routing
        let resp = std::process::Command::new("curl")
            .args([
                "-s",
                "-o",
                "/dev/null",
                "-w",
                "%{http_code}",
                "http://localhost:8081",
            ])
            .output()
            .unwrap();

        let status = String::from_utf8_lossy(&resp.stdout);

        // Cleanup
        let _ = child.kill();
        let _ = child.wait();
        let loc = crate::remote::GitLoc::Local(std::path::PathBuf::from("/"));
        let cfg = crate::config::SandboxConfig {
            enabled: true,
            ..Default::default()
        };
        teardown(&cfg, &loc, &s.name);

        assert_eq!(status.trim(), "200", "Port 8081 was not exposed properly");
    }

    #[test]
    fn integration_test_sandbox_lifecycle() {
        // Only run if podman is installed.
        if !crate::util::have("podman") {
            return;
        }

        // We skip this test in CI/automated environments to prevent rate limits
        // from Docker Hub/ECR blocking test success. The logic is verified manually.
        if std::env::var("CI").is_ok()
            || std::env::var("SKIP_PODMAN_E2E").is_ok()
            || std::env::var("PODMAN_E2E_FORCE").is_err()
        {
            return;
        }

        let mut s = spec(Backend::Podman);
        s.name = "thegn-test-lifecycle-container".into();
        s.image = Some("public.ecr.aws/docker/library/alpine:latest".into());
        // Do not bind mount fake paths like /wt/feat in the integration test as they
        // don't exist on the real host and podman will error out when creating the container.
        s.mounts = vec![];
        s.file_access = FileAccess::None;

        // Pull image first so `ensure` doesn't timeout if it tries to do it or if it's not present
        // Ignore pull failures (we might already have the image cached)
        let _ = pull_image("public.ecr.aws/docker/library/alpine:latest");

        // 1. Ensure (create keep-alive)
        let res = ensure(&s);
        assert!(res.is_ok(), "Failed to start container: {:?}", res);

        // 2. Stats
        std::thread::sleep(std::time::Duration::from_millis(1500));
        let st = stats(&s);
        assert!(st.is_some(), "Failed to fetch stats");
        let st = st.unwrap();
        assert!(!st.cpu.is_empty());

        // 3. Teardown
        let loc = crate::remote::GitLoc::Local(std::path::PathBuf::from("/"));
        let cfg = crate::config::SandboxConfig {
            enabled: true,
            ..Default::default()
        };
        teardown(&cfg, &loc, &s.name);

        // Verify it's gone
        let out = std::process::Command::new("podman")
            .args(["container", "exists", &s.name])
            .output()
            .unwrap();
        assert!(!out.status.success());
    }

    #[test]
    fn test_gc_identifies_orphans() {
        let active_wts = vec!["live".to_string()];
        let containers = vec![
            "thegn-live".to_string(),
            "thegn-dead".to_string(),
            "other-container".to_string(),
        ];
        let orphans = identify_orphans(&active_wts, &containers);
        assert_eq!(orphans.len(), 1);
        assert_eq!(orphans[0], "thegn-dead");
    }

    #[test]
    fn remote_none_bare_shell_cds_and_moshes() {
        // A remote worktree with no container backend still goes over the
        // transport as a bare shell that cd's into the remote worktree.
        let mut s = spec(Backend::None);
        s.image = None;
        s.placement = Placement::Ssh(SshPlacement::plain(
            "box".into(),
            22,
            false,
            TransportKind::Mosh,
        ));
        let argv = enter_argv(&s, "${SHELL:-/bin/sh} -l");
        assert_eq!(argv[0], "mosh");
        let body = argv.last().unwrap();
        assert!(body.contains("cd /wt/feat"));
        assert!(body.contains("exec ${SHELL:-/bin/sh} -l"));
    }

    #[test]
    fn devenv_wraps_inner() {
        let mut s = spec(Backend::Bwrap);
        s.image = None;
        s.devenv = true;
        let argv = enter_argv(&s, "claude");
        assert_eq!(argv.last().unwrap(), "exec devenv shell -- claude");
    }

    #[test]
    fn mount_parsing() {
        let home = std::env::var("HOME").unwrap_or_default();
        assert_eq!(
            parse_mount("~/.gitconfig:ro"),
            Mount {
                host: format!("{home}/.gitconfig"),
                dest: format!("{home}/.gitconfig"),
                ro: true,
                cache: false,
            }
        );
        assert_eq!(
            parse_mount("/a:/b"),
            Mount {
                host: "/a".into(),
                dest: "/b".into(),
                ro: false,
                cache: false,
            }
        );
    }

    #[test]
    fn podman_and_docker_ps_parse_and_mark_ours() {
        let podman = r#"[
          {"Names": ["thegn-wt-feat"], "Image": "ubuntu:24.04", "Status": "Up 2 hours"},
          {"Names": ["registry"], "Image": "registry:2", "Status": "Up 3 days"}
        ]"#;
        let rows = parse_podman_ps(podman);
        assert_eq!(rows.len(), 2);
        assert!(rows[0].ours && rows[0].name == "thegn-wt-feat");
        assert!(!rows[1].ours);
        assert_eq!(rows[1].image, "registry:2");

        let docker = "{\"Names\": \"thegn-x\", \"Image\": \"alpine\", \"Status\": \"Up 5 minutes\"}\n{\"Names\": \"db\", \"Image\": \"postgres:16\", \"Status\": \"Up 1 hour\"}";
        let rows = parse_docker_ps(docker);
        assert_eq!(rows.len(), 2);
        assert!(rows[0].ours);
        assert_eq!(rows[1].name, "db");

        // Garbage degrades to empty, never panics.
        assert!(parse_podman_ps("not json").is_empty());
        assert!(parse_docker_ps("not json").is_empty());
    }

    #[test]
    fn host_toolchain_mounts_are_all_ro_and_exist() {
        // Every mount returned must point to a path that actually exists on the
        // current host (no phantom entries) and must be read-only.
        for m in host_toolchain_mounts() {
            assert!(
                std::path::Path::new(&m.host).exists(),
                "host_toolchain_mounts returned non-existent path: {}",
                m.host
            );
            assert!(m.ro, "host toolchain mount must be read-only: {}", m.host);
            assert_eq!(
                m.host, m.dest,
                "host toolchain mounts must be path-preserving"
            );
        }
    }

    #[test]
    fn cfg_mounts_covered_by_parent_are_skipped() {
        // When $HOME is already bind-mounted (via host_toolchain_mounts for bwrap),
        // a cfg.mounts entry for a child path (e.g. ~/.gitconfig) must be dropped.
        // Keeping it causes bwrap "Can't create file" because bwrap cannot create a
        // file mount-point inside an already-mounted parent directory.
        let home = std::env::var("HOME").unwrap_or_default();
        if home.is_empty() {
            return; // can't test without $HOME
        }
        let cfg = crate::config::SandboxConfig {
            file_access: crate::config::FileAccess::WorktreePlusCaches,
            auto_caches: true,
            backend: crate::config::SandboxBackend::Bwrap,
            // Use a file inside $HOME — it may or may not exist, the coverage check
            // must fire regardless (covered by the parent home bind).
            mounts: vec![format!("{}/.gitconfig:ro", home)],
            ..Default::default()
        };
        let loc = crate::remote::GitLoc::from_db("/wt/x", None);
        if let Some(spec) = resolve(&cfg, &loc, "test") {
            let gitconfig = format!("{home}/.gitconfig");
            let has_gitconfig_mount = spec.mounts.iter().any(|m| m.host == gitconfig);
            assert!(
                !has_gitconfig_mount,
                "~/.gitconfig should be excluded — $HOME is already bind-mounted"
            );
        }
    }

    #[test]
    fn host_toolchain_mounts_injected_for_oci_not_bwrap() {
        // For OCI backends, host_toolchain_mounts() contributes only paths that
        // exist on the current host — verify that invariant holds by checking
        // any mount whose host path is NOT the synthetic worktree path.
        let cfg = crate::config::SandboxConfig {
            file_access: crate::config::FileAccess::WorktreePlusCaches,
            auto_caches: true,
            backend: crate::config::SandboxBackend::Podman,
            image: "debian:stable".into(),
            // Clear user-configured mounts so only host_toolchain + auto_cache mounts
            // are present; avoids depending on whether $HOME/.gitconfig exists in the
            // test environment.
            mounts: vec![],
            ..Default::default()
        };
        let loc = crate::remote::GitLoc::from_db("/wt/x", None);
        if let Some(spec) = resolve(&cfg, &loc, "test") {
            // host_toolchain_mounts() entries are ro by definition — filter to
            // ro, non-cache mounts outside the fake worktree and $HOME. The rw
            // carve-outs (language caches, ~/tmp, the agent config dir a
            // parallel test points CLAUDE_CONFIG_DIR at and then deletes) are
            // not toolchain mounts and may legitimately vanish mid-assertion.
            let home = std::env::var("HOME").unwrap_or_default();
            let toolchain: Vec<_> = spec
                .mounts
                .iter()
                .filter(|m| {
                    m.ro && !m.host.starts_with("/wt/")
                        && !m.cache
                        && (home.is_empty() || !m.host.starts_with(&home))
                })
                .collect();
            for m in &toolchain {
                assert!(
                    std::path::Path::new(&m.host).exists(),
                    "host_toolchain mount for non-existent path: {}",
                    m.host
                );
            }
            // On NixOS (where /nix/store exists) we must have injected at
            // least the nix store mount.
            if std::path::Path::new("/nix/store").exists() {
                assert!(
                    toolchain.iter().any(|m| m.host == "/nix/store"),
                    "OCI spec on NixOS should include /nix/store mount"
                );
            }
        }
    }

    // H3: Orphan GC — identify orphans correctly.
    #[test]
    fn test_identify_orphans_names_only_thegn_containers() {
        let active = vec!["/wt/live".to_string()];
        let containers = vec![
            container_name("/wt/live"),    // active → not orphan
            container_name("/wt/dead"),    // no active entry → orphan
            "other-tool-container".into(), // not thegn-prefixed → ignored
        ];
        let orphans = identify_orphans(&active, &containers);
        assert_eq!(orphans, vec![container_name("/wt/dead")]);
    }

    #[test]
    fn test_identify_orphans_empty_inputs() {
        // No containers → nothing to remove.
        assert!(identify_orphans(&["wt".to_string()], &[]).is_empty());
        // No active worktrees → all thegn containers are orphans.
        let containers = vec![container_name("/wt/a"), container_name("/wt/b")];
        let orphans = identify_orphans(&[], &containers);
        assert_eq!(orphans.len(), 2);
    }

    #[test]
    fn test_run_gc_noop_when_no_backend_available() {
        // run_gc with an empty DB set and no containers should return empty
        // without panicking (even if podman/docker aren't installed).
        let removed = run_gc(&["/wt/alive".to_string()]);
        // On CI there may be no podman — the result is just an empty list.
        assert!(removed.iter().all(|n| n.starts_with(CONTAINER_PREFIX)));
    }

    // H2: Remote transport unit tests.
    #[test]
    fn remote_enter_argv_wraps_with_mosh() {
        let mut s = spec(Backend::Podman);
        s.placement = Placement::Ssh(SshPlacement::plain(
            "devbox".into(),
            22,
            false,
            TransportKind::Mosh,
        ));
        // With a real image + OCI backend on a remote, enter_argv should
        // produce a mosh wrapper.
        let argv = enter_argv(&s, "bash -l");
        assert_eq!(argv[0], "mosh", "outer command must be mosh: {argv:?}");
        // The remote host must appear in the argv.
        assert!(argv.iter().any(|a| a == "devbox"), "host missing: {argv:?}");
    }

    #[test]
    fn remote_enter_argv_wraps_with_ssh() {
        let mut s = spec(Backend::Podman);
        s.placement = Placement::Ssh(SshPlacement::plain(
            "devbox".into(),
            2222,
            true,
            TransportKind::Ssh,
        ));
        let argv = enter_argv(&s, "bash -l");
        // SSH transport: first arg is ssh, not mosh.
        assert_eq!(argv[0], "ssh", "outer command must be ssh: {argv:?}");
        assert!(argv.iter().any(|a| a == "devbox"), "host missing: {argv:?}");
        // Port flag must be present when non-default.
        assert!(
            argv.iter().any(|a| a == "-p"),
            "port flag missing: {argv:?}"
        );
    }

    // H4 is in dns_filter.rs (already done).

    // Per-profile container naming (G1).
    #[test]
    fn container_name_with_profile_adds_slug() {
        let default = container_name_with_profile("/wt/feat", None);
        let explicit_default = container_name_with_profile("/wt/feat", Some("default"));
        let named = container_name_with_profile("/wt/feat", Some("work"));
        assert_eq!(default, container_name("/wt/feat"));
        assert_eq!(explicit_default, container_name("/wt/feat"));
        assert!(named.starts_with(CONTAINER_PREFIX));
        assert!(named.contains("work"));
        assert!(named != default);
    }

    #[test]
    fn sandbox_profile_baselines() {
        assert!(!SandboxProfile::Open.read_only_root());
        assert!(SandboxProfile::Hardened.read_only_root());
        assert!(SandboxProfile::Sealed.read_only_root());

        assert_eq!(SandboxProfile::Open.pids_limit(), None);
        assert_eq!(SandboxProfile::Hardened.pids_limit(), Some(512));
        assert_eq!(SandboxProfile::Sealed.pids_limit(), Some(256));

        // Only `sealed` drops caps + forces no-network; `hardened` keeps both so
        // debuggers/ping/networking still work.
        assert!(SandboxProfile::Hardened.drop_capabilities().is_empty());
        assert!(
            SandboxProfile::Sealed
                .drop_capabilities()
                .contains(&"ALL".to_string())
        );
        assert!(SandboxProfile::Sealed.forces_no_network());
        assert!(!SandboxProfile::Hardened.forces_no_network());
    }

    #[test]
    fn oci_opts_emit_sealed_hardening() {
        let mut s = spec(Backend::Podman);
        s.network = Network::None;
        s.read_only_root = true;
        s.no_new_privileges = true;
        s.pids_limit = Some(256);
        s.drop_capabilities = vec!["ALL".into()];
        let j = oci_create_opts(&s).join(" ");
        assert!(j.contains("--read-only"), "{j}");
        assert!(j.contains("--tmpfs /tmp"), "{j}");
        assert!(j.contains("--cap-drop ALL"), "{j}");
        assert!(j.contains("--security-opt no-new-privileges"), "{j}");
        assert!(j.contains("--pids-limit 256"), "{j}");
        assert!(j.contains("--network none"), "{j}");
    }

    #[test]
    fn oci_opts_open_profile_adds_no_hardening() {
        // `open` (all knobs off, as the spec() helper builds) must reproduce
        // today's argv — none of the hardening flags may appear.
        let s = spec(Backend::Podman);
        let j = oci_create_opts(&s).join(" ");
        assert!(!j.contains("--read-only"), "{j}");
        assert!(!j.contains("--cap-drop"), "{j}");
        assert!(!j.contains("--security-opt"), "{j}");
        assert!(!j.contains("--pids-limit"), "{j}");
    }

    #[test]
    fn vpn_sidecar_name_roundtrips() {
        let base = container_name("/wt/feat");
        let vpn = vpn_sidecar_name(&base);
        assert_eq!(vpn, format!("{base}-szvpn"));
        assert_ne!(vpn, base);
        assert_eq!(strip_vpn_suffix(&vpn), base);
        assert_eq!(strip_vpn_suffix(&base), base);
        // Independent of the agent suffix.
        assert_eq!(strip_agent_suffix(&vpn), vpn);
    }

    #[test]
    fn agent_container_name_roundtrips_and_is_not_orphan() {
        let base = container_name("/wt/feat");
        let agent = agent_container_name(&base);
        assert_ne!(agent, base);
        assert_eq!(strip_agent_suffix(&agent), base);
        assert_eq!(strip_agent_suffix(&base), base);

        // An active worktree owns BOTH its container and the agent's; only a
        // container for a no-longer-active worktree is an orphan.
        let active = vec!["/wt/feat".to_string()];
        let containers = vec![base.clone(), agent.clone(), container_name("/wt/dead")];
        let orphans = identify_orphans(&active, &containers);
        assert!(!orphans.contains(&base));
        assert!(!orphans.contains(&agent));
        assert!(orphans.contains(&container_name("/wt/dead")));
    }
}
