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
                // Reap on a DETACHED thread, never inline: a probe subprocess
                // wedged in an uninterruptible syscall (dead daemon socket,
                // stalled DNS, hung NFS) doesn't die the instant SIGKILL lands
                // — the kernel delivers it only when the task leaves the
                // syscall, so a synchronous `wait()` here blocks for as long as
                // the wedge lasts. That is exactly how one hung `podman`/`docker`
                // probe turned a 5s deadline into a multi-minute freeze on the
                // sandbox-resolution path (which pane spawns sit behind). Hand
                // the child off and return at the deadline; the zombie is
                // reaped whenever it finally dies.
                std::thread::spawn(move || {
                    let _ = child.wait();
                });
                return None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(25)),
            Err(_) => return None,
        }
    }
}

/// Run `argv` and return its **stderr**, whether or not it succeeded.
///
/// [`output_with_timeout`] pipes stdout and sends stderr to `/dev/null`, which is
/// right for the control-plane probes it serves (they are parsed for their
/// output, and a failure is just a `false`). Container *create* is the one call
/// where the diagnosis lives entirely in stderr: `podman run` exits 125 with a
/// single line naming the bind it refused, and discarding it is what left users
/// with a bare "could not start podman container '<name>'".
///
/// Returns `None` only if the process could not be spawned or hit the deadline —
/// the same three-state shape as its sibling, so a timeout is never mistaken for
/// a clean run.
pub(crate) fn stderr_with_timeout(argv: &[String], timeout: Duration) -> Option<(bool, String)> {
    use std::process::Stdio;
    let mut child = Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let err = child
                    .stderr
                    .take()
                    .and_then(|mut r| {
                        use std::io::Read;
                        let mut s = String::new();
                        r.read_to_string(&mut s).ok().map(|_| s)
                    })
                    .unwrap_or_default();
                return Some((status.success(), err));
            }
            // Detached reap, for the reason spelled out in `output_with_timeout`:
            // a wedged runtime does not die the instant SIGKILL lands, and a
            // synchronous wait here would block the pane-spawn path for as long
            // as the wedge lasts.
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                std::thread::spawn(move || {
                    let _ = child.wait();
                });
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

impl crate::seam::Probe for Backend {
    /// Cheap availability: is the backend's binary on `PATH`? (`None`/host
    /// needs nothing.) The full three-state runtime probe in
    /// `sandbox_backend::available` stays the selection-time authority —
    /// doctor only needs the offline answer.
    fn probe(&self) -> crate::seam::ProbeReport {
        use crate::seam::{Availability, ProbeReport};
        let bin = self.binary();
        let availability = if bin.is_empty() || util::which_path(bin).is_some() {
            Availability::Ready
        } else {
            Availability::Unavailable(format!("`{bin}` not found on PATH"))
        };
        ProbeReport::new("sandbox", self.label(), availability)
    }
}

impl Backend {
    /// Resolve a config-facing backend name (as used in `backend_chain` entries,
    /// e.g. `"podman-rootless"`, `"bwrap"`, `"host"`) to its concrete runtime
    /// backend. Returns `None` for unknown names.
    pub fn parse(s: &str) -> Option<Backend> {
        // One alias table: the `config_enum!`'s. A reserved kind (`wsl`)
        // parses as `None` here too — there is no runtime behind it.
        SandboxBackend::from_str_validated(s)
            .ok()
            .and_then(Backend::from_config)
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
    /// The one table every `match backend` used to re-derive: label, binary,
    /// family, and the flags the argv builders branch on. Adding a backend is
    /// one row here plus the family's argv arm.
    pub const fn profile(self) -> &'static BackendProfile {
        match self {
            Backend::Podman => &BackendProfile {
                label: "podman-rootless",
                binary: "podman",
                family: BackendFamily::Oci,
                rootful: false,
            },
            Backend::PodmanRootful => &BackendProfile {
                label: "podman-rootful",
                binary: "podman",
                family: BackendFamily::Oci,
                rootful: true,
            },
            Backend::Docker => &BackendProfile {
                label: "docker",
                binary: "docker",
                family: BackendFamily::Oci,
                rootful: true,
            },
            Backend::Smol => &BackendProfile {
                label: "smolmachines",
                binary: "smolmachines",
                family: BackendFamily::Oci,
                rootful: false,
            },
            Backend::Bwrap => &BackendProfile {
                label: "bwrap",
                binary: "bwrap",
                family: BackendFamily::Bwrap,
                rootful: false,
            },
            Backend::Systemd => &BackendProfile {
                label: "systemd",
                binary: "systemd-run",
                family: BackendFamily::Systemd,
                rootful: false,
            },
            Backend::Apple => &BackendProfile {
                label: "apple",
                binary: "container",
                family: BackendFamily::Oci,
                rootful: false,
            },
            Backend::Wsl => &BackendProfile {
                label: "wsl",
                binary: "wsl.exe",
                family: BackendFamily::Oci,
                rootful: false,
            },
            Backend::WinAppContainer => &BackendProfile {
                label: "appcontainer",
                binary: "",
                family: BackendFamily::WinAppContainer,
                rootful: false,
            },
            Backend::WinJobObject => &BackendProfile {
                label: "jobobject",
                binary: "",
                family: BackendFamily::WinJobObject,
                rootful: false,
            },
            Backend::None => &BackendProfile {
                label: "host",
                binary: "",
                family: BackendFamily::Host,
                rootful: false,
            },
        }
    }

    /// Human / config label (`podman-rootless`, `bwrap`, `host`, …).
    pub fn label(self) -> &'static str {
        self.profile().label
    }

    /// The binary probed for availability; empty for OS-native backends.
    pub fn binary(self) -> &'static str {
        self.profile().binary
    }

    /// Every backend that can hold a container ("things the GC must sweep").
    /// Derived from the profile table's family — a new OCI backend joins by
    /// construction, so the sweep can't drift by omission (the bug that let
    /// rootful-podman and `apple` containers leak). Unlike
    /// [`oci_runtimes`](Self::oci_runtimes) (the *selectable* set), this
    /// includes reserved OCI kinds such as WSL.
    pub fn all_oci() -> impl Iterator<Item = Backend> {
        Backend::ALL.into_iter().filter(|b| b.is_oci())
    }

    /// OCI-style backends run the worktree's toolchain inside an image; the
    /// others reuse the host toolchain per pane.
    pub fn is_oci(self) -> bool {
        matches!(self.profile().family, BackendFamily::Oci)
    }

    pub fn is_host_toolchain(self) -> bool {
        matches!(
            self.profile().family,
            BackendFamily::Bwrap
                | BackendFamily::Systemd
                | BackendFamily::WinAppContainer
                | BackendFamily::WinJobObject
        )
    }

    /// Have thegn's verbs for this backend been checked against the real runtime?
    ///
    /// `smol` and `wsl` are **not**. They parse, sit in [`Backend::oci_runtimes`],
    /// answer `true` from [`Backend::is_oci`], and are treated as docker clones
    /// for `--user`/`--gpus` — a complete-looking surface with nothing behind it.
    /// [`liveness_argv`] returns `None` for both, so they fall back to a bare
    /// PATH probe: **"the binary exists" stands in for "the runtime works"**,
    /// which is exactly the defect `06ec12ff` fixed for docker and Apple, where a
    /// stopped daemon was selected and then failed every pane.
    ///
    /// Neither is in [`crate::sandbox_backend::default_backend_chain`], so
    /// nothing reaches them by accident — an unverified backend is only ever
    /// something a user asked for by name, and this is what lets thegn say so
    /// instead of implying a guarantee it has not earned.
    ///
    /// The honest fix is to verify the verbs against a real install, not to
    /// invent them: guessing is how the Apple backend ended up emitting
    /// `container pull` and `container image exists`, neither of which exists.
    /// When someone does that, flip this and add a [`liveness_argv`] arm.
    pub fn verified(self) -> bool {
        !matches!(self, Backend::Smol | Backend::Wsl)
    }

    /// The OCI runtimes worth probing for a container on this host (WSL is
    /// reserved — no runtime behind it yet).
    pub fn oci_runtimes() -> impl Iterator<Item = Backend> {
        Backend::ALL
            .into_iter()
            .filter(|b| b.is_oci() && *b != Backend::Wsl)
    }

    /// Every runtime backend (for tables and coverage tests).
    pub const ALL: [Backend; 11] = [
        Backend::Podman,
        Backend::PodmanRootful,
        Backend::Docker,
        Backend::Smol,
        Backend::Bwrap,
        Backend::Systemd,
        Backend::Apple,
        Backend::Wsl,
        Backend::WinAppContainer,
        Backend::WinJobObject,
        Backend::None,
    ];
}

/// How a backend is driven. The argv builders branch on this, not on the
/// eleven variants, so the OCI backends share one arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendFamily {
    /// `podman` / `docker` / `smolmachines` / Apple `container` / WSL: a
    /// container image with the worktree bind-mounted at its real path.
    Oci,
    /// bubblewrap namespace, host toolchain.
    Bwrap,
    /// `systemd-run` transient scope, host toolchain.
    Systemd,
    /// Windows AppContainer.
    WinAppContainer,
    /// Windows Job Object.
    WinJobObject,
    /// No sandbox.
    Host,
}

/// A backend's static facts (see [`Backend::profile`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackendProfile {
    pub label: &'static str,
    pub binary: &'static str,
    pub family: BackendFamily,
    /// Runs as root on the host (rootful podman, docker).
    pub rootful: bool,
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
    /// Aggregate memory across all panes, on the same shared slice. `MemoryHigh`
    /// (throttle + reclaim), not `MemoryMax` (OOM-kill) — see the config field.
    /// `None` = no aggregate memory cap.
    pub memory_total: Option<String>,
}

/// The config `[sandbox.limits]` table as resolved ceilings. A plain field copy
/// — the two structs are deliberately separate (config is serde/schemars, this
/// one is the substrate-free runtime value) — but centralized here so a new
/// ceiling is wired once, and so callers outside `spec_for` (the merge-queue
/// gate, the agent handoff) can reach the same mapping.
impl From<&crate::config::SandboxLimits> for SandboxLimits {
    fn from(c: &crate::config::SandboxLimits) -> Self {
        SandboxLimits {
            cpu: c.cpu.clone(),
            memory: c.memory.clone(),
            cpu_total: c.cpu_total.clone(),
            memory_total: c.memory_total.clone(),
        }
    }
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
    /// OCI runtime to run the worktree container under (`[sandbox] oci_runtime`):
    /// e.g. `"runsc"` (gVisor userspace kernel) or `"krun"` (libkrun microVM).
    /// `None` ⇒ the daemon default (`runc`/`crun`, shared-kernel). Injected as
    /// `--runtime <value>` at container *create* by `oci_create_opts` for OCI
    /// backends only; drives the honest isolation class in [`crate::capabilities`].
    pub oci_runtime: Option<String>,
    /// This sandbox is owned by the **pane daemon** (a separate, long-lived
    /// process that keeps the shell running across UI detach — "tmux
    /// semantics"). The daemon reaps its sessions explicitly (kill-on-close,
    /// lease expiry, boot sweep), so the bwrap `--die-with-parent` guard is
    /// both redundant and actively harmful here: it tied the sandbox's life to
    /// the transient thread that forked it, killing a *supposed-to-persist*
    /// backgrounded shell. Set on daemon-routed center-tab panes only;
    /// ephemeral in-process chrome panes (pins/drawer) leave it `false` so they
    /// still die with the compositor. Only affects the bwrap backend.
    pub daemon_persistent: bool,
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

/// Like [`resolve`] but with an explicit hardening [`SandboxProfile`], for
/// callers that need a preset other than the config's `profile`.
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
    resolve_placed_with(cfg, loc, name, profile, placement, Fallthrough::Chain)
}

/// [`resolve_placed`] that answers for `cfg.backend` **only** — no degrade into
/// `backend_chain`, no host-fallback message. For the spawn path, which iterates
/// the chain itself; see [`Fallthrough::Exact`] for why composing the two walks
/// was an N² probe storm.
pub fn resolve_placed_exact(
    cfg: &SandboxConfig,
    loc: &GitLoc,
    name: &str,
    profile: SandboxProfile,
    placement: Placement,
) -> Option<SandboxSpec> {
    resolve_placed_with(cfg, loc, name, profile, placement, Fallthrough::Exact)
}

fn resolve_placed_with(
    cfg: &SandboxConfig,
    loc: &GitLoc,
    name: &str,
    profile: SandboxProfile,
    placement: Placement,
    mode: Fallthrough,
) -> Option<SandboxSpec> {
    if !cfg.enabled {
        return None;
    }
    let backend = pick_backend_with(cfg, &placement, mode)?;
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
    //
    // **Only when the host and the guest share an ABI.** An OCI container is
    // always a *Linux* guest; `host_toolchain_mounts` hands it the *host's*
    // `/usr`, `/bin`, `/lib` and `/nix/store`. On a Linux host those are Linux
    // binaries and the whole scheme works — that is what it was built for. On a
    // Mac they are Mach-O, and mounting them over the guest's own directories
    // does not merely fail to help, it breaks the container outright:
    //
    //   -v /usr:/usr:ro  → "failed to find target executable sleep"
    //   -v /bin:/bin:ro  → "Exec format error"   (the guest's /bin/sh is now Mach-O)
    //
    // (Both verified against Apple's `container` on macOS 26; the same applies to
    // podman/docker there, whose Linux guests live in a VM.) bwrap is unaffected
    // — it is a Linux-host namespace tool, so host and guest are the same system
    // by construction.
    let same_abi_as_guest = guest_shares_host_abi(backend, crate::sandbox_backend::host_os());
    let inject_host_toolchain =
        (backend.is_oci() || backend == Backend::Bwrap) && cfg.auto_caches && same_abi_as_guest;
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

    // THE-66: sealed / sealed-tunnel tiers get a default-deny secret posture —
    // the SSH agent socket is dropped from the passthrough even if the config
    // still names it (a sealed pane with the user's agent contradicts the tier).
    // Explicit `env_passthrough` on those tiers still re-adds it; doctor flags
    // it. Hardened/open keep whatever the passthrough names.
    let seal_agent = profile.seals_agent_socket();
    let mut env: Vec<(String, String)> = cfg
        .env_passthrough
        .iter()
        .filter(|k| !(seal_agent && k.as_str() == "SSH_AUTH_SOCK"))
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

    // prek's `pre-commit` hook lives in the shared `.git/hooks` and so fires in
    // every sandbox, but its `.pre-commit-config.yaml` is a gitignored,
    // devenv-generated nix-store symlink that only exists in the checkout where
    // `devenv shell` ran. When a sandbox shell has it (direnv/devenv active) the
    // hooks run normally; when it doesn't, this makes prek skip rather than abort
    // the commit — no dependency on store-mount timing or `post-checkout`
    // symlink-chaining lining up. The real gate is pre-push (clippy/test/smoke).
    env.push(("PREK_ALLOW_NO_CONFIG".to_string(), "1".to_string()));

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
    //
    // Withheld across an ABI boundary (see [`guest_shares_host_abi`]): a Mac's
    // nix-daemon serves *darwin* store paths, which a Linux guest cannot execute
    // — and the bind fails the create outright, because podman machine does not
    // share `/nix`. This is the same gate the host-toolchain mounts already use;
    // it reaches here because the socket is injected separately from them.
    let abi_ok = guest_shares_host_abi(backend, crate::sandbox_backend::host_os());
    let auto_daemon = placement.is_local()
        && !profile.forces_no_network()
        && cfg.warm_direnv != crate::config::WarmDirenv::Off
        && crate::direnv::has_flake_envrc(&worktree);
    if !abi_ok && cfg.nix_daemon {
        // Explicitly requested and dropped: say so, per the same rule
        // `unsupported_hardening` follows — never ship a quietly different
        // sandbox than the config asked for.
        msg::warn(&format!(
            "sandbox: [sandbox] nix_daemon is on, but a {} container is a Linux guest on this \
             host — the host Nix daemon serves host-native store paths it cannot run, and \
             binding /nix fails the container. Leaving it off.",
            backend.label()
        ));
    }
    if abi_ok && (cfg.nix_daemon || auto_daemon) {
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
        limits: SandboxLimits::from(&cfg.limits),
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
        oci_runtime: (!cfg.oci_runtime.trim().is_empty())
            .then(|| cfg.oci_runtime.trim().to_string()),
        // Default off: the resolver has no idea whether this pane is a
        // daemon-routed center tab or an ephemeral chrome pane. The pane owner
        // (the host's launch-spec builder) flips it on for daemon-backed panes.
        daemon_persistent: false,
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

/// LEGACY suffix that marked the (since-removed) embedded agent's own
/// separately-hardened container. Kept so teardown/reconciliation still cleans
/// up containers created by older builds. Chosen to be collision-resistant
/// against worktree slugs that happen to end in `-agent`.
pub const AGENT_CONTAINER_SUFFIX: &str = "-tgagent";

/// The legacy agent container name, derived from the worktree container name
/// `base` — only used to `rm -f` leftovers from older builds.
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
pub const VPN_SIDECAR_SUFFIX: &str = "-tgvpn";

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

/// The running containers, thegn-owned first, **without** per-container stats.
/// Probes rootless podman, rootful podman, then docker; one fast `ps`
/// subprocess per backend on the caller's (background) thread. Empty when no
/// OCI runtime is installed.
///
/// This is the cheap ambient listing (names, image, status/health) the Sandbox
/// panel section and container chip need on their 5s cadence. The expensive
/// `stats --no-stream` enrichment is [`running_containers_with_stats`], run only
/// while a surface that displays per-container CPU/mem is visible (the monitor's
/// Containers tab, or the Sandbox section's expanded stats) — see the host's
/// visibility gate. Running `stats` unconditionally every 5s forever was a
/// standing cost this split removes.
pub fn running_containers() -> Vec<ContainerInfo> {
    running_containers_impl(false)
}

/// [`running_containers`] plus per-container CPU/mem/net from `stats
/// --no-stream` (one extra subprocess per backend — on docker it can take over a
/// second). Call only behind the visibility gate.
pub fn running_containers_with_stats() -> Vec<ContainerInfo> {
    running_containers_impl(true)
}

fn running_containers_impl(with_stats: bool) -> Vec<ContainerInfo> {
    let mut out = Vec::new();
    if let Some(stdout) = run_local_output(
        &backend_prefix(Backend::Podman),
        &["ps", "--format", "json"],
    ) {
        let mut rows = parse_podman_ps(&stdout);
        if with_stats {
            apply_stats(&mut rows, &oci_stats(Backend::Podman));
        }
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
        if with_stats {
            apply_stats(&mut rows, &oci_stats(Backend::PodmanRootful));
        }
        out.extend(rows);
    }
    if out.is_empty()
        && let Some(stdout) = run_local_output(
            &backend_prefix(Backend::Docker),
            &["ps", "--format", "{{json .}}"],
        )
    {
        let mut rows = parse_docker_ps(&stdout);
        if with_stats {
            apply_stats(&mut rows, &oci_stats(Backend::Docker));
        }
        out.extend(rows);
    }
    out.sort_by_key(|c| (!c.ours, c.name.clone()));
    out
}

/// Aggregate disk footprint of thegn's container estate (the Containers-tab
/// header). Runs `system df` + owned image/volume listings + `ps -a` across the
/// detected docker/podman backends — one subprocess set per backend, bounded by
/// `PROBE_TIMEOUT`. Impure (subprocesses live here, beside `running_containers`);
/// the argv builders and parsers it composes are the pure `sandbox_manage` half.
///
/// Call only behind the visibility gate and on a slow cadence — `docker system
/// df` walks the layer stores. Backends whose daemon is down are skipped (the
/// probe cache makes that cheap); an `apple` engine present marks the total
/// partial (it has no `df` op).
pub fn container_footprint() -> crate::sandbox_manage::ContainerFootprint {
    use crate::sandbox_manage as m;
    let mut fp = m::ContainerFootprint::default();
    let run = |prefix: &[String], argv: &[String]| -> Option<String> {
        let a: Vec<&str> = argv.iter().map(String::as_str).collect();
        run_local_output(prefix, &a)
    };
    for backend in [Backend::Podman, Backend::PodmanRootful, Backend::Docker] {
        if available(&Placement::Local, backend) != RuntimeProbe::Present {
            continue;
        }
        let prefix = backend_prefix(backend);
        if let Some(argv) = m::mgmt_list_argv(backend)
            && let Some(out) = run(&prefix, &argv)
        {
            let rows = if backend == Backend::Docker {
                parse_docker_ps(&out)
            } else {
                parse_podman_ps(&out)
            };
            fp.containers += rows.iter().filter(|c| c.ours).count() as u64;
        }
        if let Some(argv) = m::mgmt_image_list_argv(backend)
            && let Some(out) = run(&prefix, &argv)
        {
            fp.images += m::parse_owned_images(&out).len() as u64;
        }
        if let Some(argv) = m::mgmt_volume_list_argv(backend)
            && let Some(out) = run(&prefix, &argv)
        {
            fp.volumes += m::parse_owned_volumes(&out).len() as u64;
        }
        match m::mgmt_df_argv(backend).and_then(|argv| run(&prefix, &argv)) {
            Some(out) => {
                let du = m::parse_system_df(&out);
                fp.disk.images.0 += du.images.0;
                fp.disk.images.1 += du.images.1;
                fp.disk.containers.0 += du.containers.0;
                fp.disk.containers.1 += du.containers.1;
                fp.disk.volumes.0 += du.volumes.0;
                fp.disk.volumes.1 += du.volumes.1;
            }
            None => fp.partial = true,
        }
    }
    // An apple engine owns containers but has no `df` op — the total is a floor.
    if available(&Placement::Local, Backend::Apple) == RuntimeProbe::Present {
        fp.partial = true;
    }
    fp
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
        // Carry the target's ssh knobs into the placement rather than rebuilding
        // from the bare triple: `SshPlacement::plain` leaves them `None`, so a
        // target reachable only with `-i`/`-F` (any local VM) would produce a
        // placement that cannot connect — while the *control-plane* reads for the
        // same worktree, which go through `SshTarget::ssh_base`, still could.
        // Two commands for one host that disagree on how to reach it is the
        // hardest kind of this bug to see.
        let mut p = SshPlacement::plain(ssh.host.clone(), ssh.port, ssh.forward_agent, kind);
        p.ssh_config = ssh.ssh_config.clone();
        p.jump_host = ssh.jump_host.clone();
        p.identity = ssh.identity.clone();
        p.extra_args = ssh.extra_args.clone();
        Placement::Ssh(p)
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

use crate::sandbox_backend::{Fallthrough, HostOs, available, pick_backend_with};
pub use crate::sandbox_backend::{ProbePass, placement_reachable, probe_pass_guard};

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

/// `(running, mounts_ok)` parsed from Apple `container inspect`'s JSON.
///
/// Apple's CLI diverges from docker/podman on both halves of the probe, and
/// silently: `container container inspect` is not a command ("Plugin
/// 'container-container' not found" — its `inspect` is top-level, with no noun),
/// and it has **no Go templates** at all (`--format` is an unknown option).
/// Running the docker-shaped probe against it therefore always answered "not
/// running", so a container thegn had just successfully created was declared a
/// failure and the backend fell out of the chain — with the container left
/// running. `gc_list_argv` already documents the same "no `ps`, no templates"
/// divergence for the sweep; this is its `inspect` twin.
///
/// Shape (verified against `container` 1.2.2):
/// `[{ "status": { "state": "running" }, "configuration": { "mounts": [ { "source": … } ] } }]`
pub(crate) fn parse_apple_inspect(stdout: &str, required: &[&str]) -> (bool, bool) {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(stdout) else {
        return (false, false);
    };
    let Some(first) = v.as_array().and_then(|a| a.first()) else {
        return (false, false);
    };
    let running = first
        .get("status")
        .and_then(|s| s.get("state"))
        .and_then(|s| s.as_str())
        .is_some_and(|s| s.eq_ignore_ascii_case("running"));
    if !running {
        return (false, false);
    }
    let active: std::collections::HashSet<&str> = first
        .get("configuration")
        .and_then(|c| c.get("mounts"))
        .and_then(|m| m.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|m| m.get("source")?.as_str())
                .collect()
        })
        .unwrap_or_default();
    let mounts_ok = required.iter().all(|r| active.contains(*r));
    (true, mounts_ok)
}

/// Force-remove this spec's container on the SAME daemon `run -d` creates it on.
///
/// Must go through `oci_prefix` (which carries `sudo -n` for rootful, and
/// `--url`/`--connection`/`-H` for `oci_host`): a bare `<rt> rm` hits the local
/// rootless store and silently no-ops on a rootful or remote container, after
/// which the recreate fails "name in use".
///
/// Fires even for a `daemon_persistent` spec. A container whose bind is wrong
/// can never become right — its mount namespace was fixed at create — so
/// persisting it is strictly worse than paying for a recreate.
pub(crate) fn remove_container(spec: &SandboxSpec) {
    let mut rm = oci_prefix(spec);
    rm.extend(["rm".into(), "-f".into(), spec.name.clone()]);
    // best-effort: a missing container is a harmless no-op, and the caller is
    // about to recreate or fail anyway.
    let _ = run_control_owned(spec, &rm, PROBE_TIMEOUT);
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
        // Apple's `container` has no `container` noun and no Go templates — see
        // `parse_apple_inspect`. Ask it in its own dialect and parse the JSON.
        if spec.backend == Backend::Apple {
            argv.extend(["inspect".into(), spec.name.clone()]);
            let Some((ok, stdout)) = output_with_timeout(&argv, PROBE_TIMEOUT) else {
                return (false, false); // timed out
            };
            if !ok && stdout.is_empty() {
                return (false, false); // container doesn't exist
            }
            let req: Vec<&str> = required.iter().copied().collect();
            return parse_apple_inspect(&stdout, &req);
        }
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
        remove_container(spec);
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
    // Keep the create's stderr: it is the only place the runtime says WHY, and
    // for the commonest macOS failure (a bind the VM cannot resolve) it names the
    // exact path. `run_control_owned` would drop it.
    let create_err = stderr_with_timeout(&spec.placement.control_argv(&argv), RUN_TIMEOUT)
        .map(|(_, e)| e)
        .unwrap_or_default();
    // Don't trust the exit code of `podman run -d`: on NixOS with broken
    // --userns keep-id, crun exits 0 but leaves the container in "created"
    // state. Verify it is actually running before declaring success.
    if container_status(spec).0 {
        emit(SandboxPhase::PhaseDone);
        return Ok(());
    }

    // A refused bind is terminal for this spec: the mount set is what the runtime
    // rejected, and every retry below varies something else (the user namespace),
    // so retrying only rediscovers the next unshared path and still ends in a
    // generic error. Fail now, naming the path and the fix.
    if let Some(missing) = crate::sandbox_mountcheck::parse_unshared_bind(&create_err) {
        let failure = crate::sandbox_mountcheck::mount_failure(
            &crate::sandbox_mountcheck::MountProbe {
                backend: spec.backend,
                os: crate::sandbox_backend::host_os(),
                file_access: spec.file_access,
                worktree: &crate::sandbox_preflight::canonical_worktree(spec),
                missing,
            },
            &|b| crate::util::have(b),
        );
        emit(SandboxPhase::PhaseFailed {
            err: failure.headline.clone(),
        });
        anyhow::bail!(failure.one_line())
    }

    // Some rootless Podman/crun combinations (seen on NixOS) fail every
    // container started with `--userns keep-id` even though ordinary rootless
    // containers work. Retry without keep-id so an explicit rootless Podman
    // selection still produces a real container instead of forcing host use.
    if spec.backend == Backend::Podman {
        remove_container(spec);
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
    // Also remove any LEGACY separate agent container (`thegn-{slug}-tgagent`,
    // created by older builds); `rm -f` of a non-existent name is a harmless
    // no-op.
    let agent = agent_container_name(&name);
    // Also remove the VPN sidecar (`thegn-{slug}-tgvpn`) when one was started;
    // `rm -f` of a missing name is a harmless no-op. (Ephemeral node de-register
    // is the host's job via `thegn-svc::vpn::down` before this runs.)
    let vpn = vpn_sidecar_name(&name);
    let placement = Placement::Local;
    for b in Backend::oci_runtimes() {
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
    // Remove both the worktree container and any LEGACY separate agent
    // container left behind by older builds; `rm -f` of a missing name is a
    // harmless no-op.
    let agent = agent_container_name(name);
    let vpn = vpn_sidecar_name(name);
    // Drive the same remote daemon the create used, or `None` for the local
    // daemon; without this an `[sandbox] oci_host` container is never removed
    // (rm hits the local daemon, which never held it) and leaks forever.
    let oci_host = (!cfg.oci_host.trim().is_empty()).then(|| cfg.oci_host.trim());
    // Try whichever OCI runtimes are available; the container only exists under one.
    for b in Backend::oci_runtimes() {
        if available(&placement, b) == RuntimeProbe::Present {
            let mut argv = oci_prefix_for(b, oci_host);
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

/// Stop or restart a container by name (OCI backends): `action` is `"stop"`
/// or `"restart"`. `backend` is the container's own runtime (from the probe
/// row) so the command goes straight to the right daemon rather than the
/// teardown-style fan-out. Runs on the worktree's host (local or remote, per
/// its `GitLoc`). Best-effort; returns whether the runtime acknowledged.
/// Subprocess seam — exercised by the Sandbox panel's `s`/`r` keys.
pub fn container_control(
    cfg: &SandboxConfig,
    loc: &GitLoc,
    backend: Backend,
    name: &str,
    action: &str,
) -> bool {
    let placement = placement_from_loc(cfg, loc);
    let oci_host = (!cfg.oci_host.trim().is_empty()).then(|| cfg.oci_host.trim());
    let mut argv = oci_prefix_for(backend, oci_host);
    argv.extend([action.to_string(), name.to_string()]);
    // `stop` honors the container's stop-grace (default 10s) before SIGKILL,
    // and `restart` pays it twice — give it comfortably more than that.
    let _timeout = Duration::from_secs(45);
    run_control_t_owned(&placement, &argv, _timeout).unwrap_or(false)
}

/// The argv that tails a container's logs (`<runtime> logs --tail 200 -f`),
/// for the host to run in an interactive pane. Pure.
pub fn container_logs_argv(cfg: &SandboxConfig, backend: Backend, name: &str) -> Vec<String> {
    let oci_host = (!cfg.oci_host.trim().is_empty()).then(|| cfg.oci_host.trim());
    let mut argv = oci_prefix_for(backend, oci_host);
    argv.extend([
        "logs".into(),
        "--tail".into(),
        "200".into(),
        "-f".into(),
        name.to_string(),
    ]);
    argv
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
    } else if inner.contains("&&")
        || inner.contains("||")
        || inner.contains('|')
        || inner.contains(';')
        || inner.contains('\n')
    {
        // Compound expressions (e.g. a shell probe chain like
        // `command -v zsh && exec zsh -l; exec bash -l`, or an `||` fallback
        // like `claude --resume || claude`) must NOT be prefixed with `exec` —
        // `exec` only accepts a single simple command, and a failed `exec`
        // (command not found) exits the shell before any `||`/`;` fallback can
        // run. Running the expression directly lets the individual `exec` calls
        // inside it handle process replacement. (`||` is caught before the bare
        // `|` check, but either substring is enough to skip the prefix.)
        s.push_str(inner);
    } else {
        s.push_str(&format!("exec {inner}"));
    }
    s
}

/// The backend-specific argv that runs `/bin/sh -lc <script>` in the sandbox.
fn backend_enter_argv(spec: &SandboxSpec, script: &str) -> Vec<String> {
    let wt = spec.worktree.to_string_lossy().into_owned();
    // Keyed on the backend *family* (one arm per way of driving a sandbox),
    // so a new OCI runtime is a profile row, not a new arm here.
    match spec.backend.profile().family {
        BackendFamily::Oci => {
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
        BackendFamily::WinAppContainer | BackendFamily::WinJobObject => {
            // These native Windows backends run the standard command, optionally
            // wrapperized by internal logic if requested, but from the process builder
            // perspective they just run the script through the user shell in cwd.
            // When spawn_with_env runs it, we could intercept and wrap in a job object.
            // For argv generation, we just emit the plain shell command since the real
            // isolation happens in the OS process creation syscalls.
            crate::shellinv::run_argv(&util::shell(), script)
        }
        BackendFamily::Bwrap => {
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
            v.push("--unshare-pid".into());
            // `--die-with-parent` kills the sandbox (bwrap is PID 1 of the
            // unshared namespace) the instant its parent goes away — right for
            // an in-process pane owned by the compositor, but fatal for a
            // daemon-owned shell that is *supposed* to survive UI detach: the
            // guard is keyed to the transient thread that forked bwrap, so it
            // reaps a backgrounded session. The pane daemon reaps its own
            // sessions explicitly (kill-on-close + lease expiry + boot sweep),
            // so drop the flag for daemon-persistent panes.
            if !spec.daemon_persistent {
                v.push("--die-with-parent".into());
            }
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
        BackendFamily::Systemd => {
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
            // Host-sourced secrets go to a 0600 `EnvironmentFile=` property so
            // they stay off systemd-run's argv, which persists for the pane's
            // whole lifetime in `/proc/*/cmdline`; synthetic/non-secret pairs
            // stay inline as `--setenv K=V`. (Remote specs keep everything
            // inline — see `partition_secret_env`.)
            let (inline_env, secret_env) = partition_secret_env(spec);
            if let Some(envfile) = write_secret_env_file(&spec.name, &secret_env) {
                v.extend([
                    "-p".into(),
                    format!("EnvironmentFile={}", envfile.display()),
                ]);
            } else {
                for (k, val) in &secret_env {
                    v.extend(["--setenv".into(), format!("{k}={val}")]);
                }
            }
            for (k, val) in &inline_env {
                v.extend(["--setenv".into(), format!("{k}={val}")]);
            }
            v.extend(["/bin/sh".into(), "-lc".into(), script.to_string()]);
            v
        }
        BackendFamily::Host => {
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

/// Split `spec.env` into (non-secret inline pairs, secret pairs) for a *local*
/// Borrowed `(key, value)` env pairs from a spec's `env` map.
type EnvPairs<'a> = Vec<(&'a String, &'a String)>;

/// spec. A pair whose value matches the launcher's own process env is a
/// host-sourced passthrough secret (GH_TOKEN, API keys — see `resolve`'s
/// env_passthrough) and MUST NOT ride the world-readable argv (`-e K=V` /
/// `--setenv K=V` are visible in `/proc/*/cmdline`); it goes to a 0600
/// env-file instead. Synthetic pairs (THEGN_SANDBOX, NIX_REMOTE — values absent
/// from the host env) are not secrets and stay inline. A REMOTE spec returns all
/// pairs inline: the argv is the only env carrier through ssh (mirrors the bwrap
/// branch), so nothing is diverted.
fn partition_secret_env(spec: &SandboxSpec) -> (EnvPairs<'_>, EnvPairs<'_>) {
    let mut inline: EnvPairs = Vec::new();
    let mut secret: EnvPairs = Vec::new();
    let local = spec.placement.is_local();
    for (k, val) in &spec.env {
        if local && std::env::var(k).ok().as_deref() == Some(val) {
            secret.push((k, val));
        } else {
            inline.push((k, val));
        }
    }
    (inline, secret)
}

/// Write host-sourced secret env pairs to a stable, 0600 per-sandbox env-file
/// (`$XDG_STATE_HOME/thegn/sandbox-env/<name>.env`) and return its path, so the
/// OCI `--env-file` / systemd `EnvironmentFile=` flags can carry tokens off the
/// world-readable argv. Returns `None` when there are no secrets to write or the
/// file can't be created (caller then keeps the pairs inline — availability over
/// a hard failure). The 0600 mode is set BEFORE the secret bytes are written.
fn write_secret_env_file(name: &str, secret: &[(&String, &String)]) -> Option<PathBuf> {
    if secret.is_empty() {
        return None;
    }
    let dir = util::xdg_state_home().join("thegn/sandbox-env");
    std::fs::create_dir_all(&dir).ok()?;
    let _ = crate::fsperm::restrict_dir_to_owner(&dir);
    let path = dir.join(format!("{name}.env"));
    // Create empty + lock down to 0600 before writing any secret bytes, so the
    // token never lands in a world-readable file even momentarily.
    std::fs::File::create(&path).ok()?;
    crate::fsperm::restrict_to_owner(&path).ok()?;
    let body: String = secret.iter().map(|(k, v)| format!("{k}={v}\n")).collect();
    std::fs::write(&path, body).ok()?;
    Some(path)
}

/// OCI `run` options shared by the keep-alive container: mounts, network, env,
/// and uid mapping so bind-mounted files stay host-owned.
fn oci_create_opts(spec: &SandboxSpec) -> Vec<String> {
    let mut v = Vec::new();
    // Ownership marker: every container thegn creates carries `thegn.managed`,
    // so container-management (the Containers tab, `sandbox prune`) has one label
    // for containers, images and volumes alike. The `thegn-` name prefix already
    // identifies the existing estate; this converges the marker so a future
    // label-based query is exact. (The seed helper container + warm volumes are
    // already labelled at provisioning — `OciRunner::seed_volume`.)
    //
    // Skipped for Apple's `container run`: its arg parser rejects unknown flags
    // with EX_USAGE (the same reason `--security-opt`/`--pids-limit` are dropped
    // for it), and `--label` is unverified there — Apple containers stay owned by
    // their `thegn-` name, which is what management uses for them anyway.
    if spec.backend != Backend::Apple {
        v.extend(["--label".into(), crate::sandbox_manage::OWNED_LABEL.into()]);
    }
    // Run under a specific OCI runtime (gVisor's `runsc`, libkrun's `krun`, …)
    // when requested. podman/docker persist the runtime in the container config,
    // so only `create` needs the flag — `exec`/`inspect`/teardown via `oci_prefix`
    // pick it up automatically.
    if spec.backend.is_oci()
        && let Some(rt) = spec
            .oci_runtime
            .as_deref()
            .map(str::trim)
            .filter(|r| !r.is_empty())
    {
        v.extend(["--runtime".into(), rt.to_string()]);
    }
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
    //
    // Same ABI gate as the toolchain mounts and the daemon socket: on a Mac the
    // store holds Mach-O, so the "resolved absolute path" the guest would exec is
    // unrunnable there — and `/nix` is outside the VM's shared set, so the bind
    // fails the create before that ever matters.
    if spec.devenv
        && let Some(p) = &spec.devenv_path
        && p.starts_with("/nix")
        && std::path::Path::new("/nix").exists()
        && guest_shares_host_abi(spec.backend, crate::sandbox_backend::host_os())
    {
        v.extend(["-v".into(), "/nix:/nix:ro".into()]);
    }
    // Host-sourced secrets (tokens, API keys) go to a 0600 `--env-file` so they
    // never land on the world-readable process argv; only synthetic/non-secret
    // pairs stay inline as `-e K=V`. (Remote specs keep everything inline — the
    // argv is the only env carrier through ssh; see `partition_secret_env`.)
    let (inline_env, secret_env) = partition_secret_env(spec);
    if let Some(envfile) = write_secret_env_file(&spec.name, &secret_env) {
        v.extend(["--env-file".into(), envfile.to_string_lossy().into_owned()]);
    } else {
        // No env-file (no secrets, or write failed) — keep secrets inline rather
        // than silently drop them; correctness over the leak in that rare case.
        for (k, val) in &secret_env {
            v.extend(["-e".into(), format!("{k}={val}")]);
        }
    }
    for (k, val) in &inline_env {
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
        v.extend(["--cpus".into(), cpu_limit_for(spec.backend, c)]);
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
    // `--security-opt`/`--pids-limit` are docker/podman spellings that Apple's
    // `container run` rejects outright (exit 64), so gate them on the backend
    // rather than the profile. What was dropped is reported via
    // `unsupported_hardening`, never swallowed.
    if backend_supports_proc_hardening(spec.backend) {
        if spec.no_new_privileges {
            v.extend(["--security-opt".into(), "no-new-privileges".into()]);
        }
        if let Some(p) = spec.pids_limit {
            v.extend(["--pids-limit".into(), p.to_string()]);
        }
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

/// Does `backend`'s guest run the same OS and ABI as this host?
///
/// Only OCI backends can have a foreign guest: their container is *always* a
/// Linux one, so on a Mac (or Windows) it lives inside a Linux VM while the host
/// binaries are Mach-O (or PE). Every other backend — bwrap, systemd, `none` —
/// executes on this host's own kernel, so host and guest are the same system by
/// construction and the answer is yes regardless of OS.
///
/// This gates the three places thegn injects **host** toolchain paths into a
/// container: [`crate::sandbox_mounts::host_toolchain_mounts`], the Tier-B Nix
/// daemon socket, and the devenv `/nix` bind. All three exist so the user's real
/// shell and toolchain work unchanged inside the sandbox, and all three are
/// nonsense across an ABI boundary — a Mach-O `/nix/store` cannot execute in a
/// Linux guest, and a darwin nix-daemon serves darwin store paths.
///
/// They also **fail the container outright** rather than merely not helping:
/// `-v /bin:/bin:ro` overmounts the guest's own `/bin/sh` with Mach-O ("Exec
/// format error"), and on macOS the VM shares only a fixed set of host
/// directories, so binding an unshared one (`/nix` is not in podman machine's
/// `/Users`, `/private`, `/var/folders`) makes `run` exit 125 with
/// `statfs /nix: no such file or directory` before any container exists.
pub(crate) fn guest_shares_host_abi(backend: Backend, os: HostOs) -> bool {
    !backend.is_oci() || os == HostOs::Linux
}

pub(crate) fn backend_prefix(backend: Backend) -> Vec<String> {
    match backend {
        Backend::PodmanRootful => vec!["sudo".into(), "-n".into(), "podman".into()],
        _ => vec![backend.binary().into()],
    }
}

/// The cheap "is this runtime actually usable?" command for `backend`, appended
/// to [`backend_prefix`]. `None` ⇒ no liveness verb is known, so a PATH-presence
/// probe stands in.
///
/// Presence on PATH is NOT usability for a client/daemon runtime: the `docker`
/// CLI installs happily with `dockerd` stopped or Docker Desktop quit, and
/// `brew install container` leaves Apple's binary on PATH with its service (and
/// its VM kernel) uninstalled. Both then answered "available", got selected, and
/// failed EVERY pane downstream in `sandbox_prefetch::prefetch_image` — a broken
/// editor whose real cause never reached the user. `podman-rootful` already had
/// to do this (rootful can't be seen from PATH at all); this generalises it.
///
/// `None` for `bwrap`/`systemd` is deliberate and permanent: they are process
/// wrappers with no daemon, so being on PATH *is* being usable. `smol`/`wsl` are
/// `None` pending someone verifying their verbs against the real runtimes —
/// guessing here would regress a backend that currently works.
pub(crate) fn liveness_argv(backend: Backend) -> Option<Vec<&'static str>> {
    match backend {
        // `version` talks to the daemon/service, so it fails when one is down.
        Backend::Podman | Backend::PodmanRootful | Backend::Docker => Some(vec!["version"]),
        // Apple's own answer to "are the services up?" — it is also what the CLI
        // tells you to run when they are not.
        Backend::Apple => Some(vec!["system", "status"]),
        Backend::Bwrap | Backend::Systemd => None,
        Backend::Smol | Backend::Wsl => None,
        Backend::WinAppContainer | Backend::WinJobObject | Backend::None => None,
    }
}

/// Whether `backend`'s `run` accepts the docker/podman hardening flags
/// `--security-opt` and `--pids-limit`.
///
/// Apple's `container run` accepts `-v`, `--tmpfs`, `--cap-add`/`--cap-drop`,
/// `--read-only`, `--memory` and `--cpus`, but has **no** `--security-opt` and
/// **no** `--pids-limit`. Emitting either makes the create exit 64 (EX_USAGE),
/// so the default `hardened` profile could never start an `apple` container even
/// once the pull verb was right. The per-container Linux VM already earns
/// [`crate::capabilities::IsolationClass::GuestKernel`], so dropping the two
/// in-guest knobs is a narrowing we can afford — but the caller must SAY so (see
/// [`unsupported_hardening`]) rather than silently ship a weaker profile.
pub(crate) fn backend_supports_proc_hardening(backend: Backend) -> bool {
    backend != Backend::Apple
}

/// The hardening knobs `spec` asked for that `spec.backend` cannot express, as
/// user-facing flag names. Empty when nothing was dropped.
///
/// Surfaced through the sandbox `warnings` vec so `thegn doctor` and the Sandbox
/// panel report the profile that is actually in force, not the one requested —
/// the same rule `display observed containment, not the recorded pick` already
/// applies to the backend itself.
pub fn unsupported_hardening(spec: &SandboxSpec) -> Vec<&'static str> {
    let mut out = Vec::new();
    if backend_supports_proc_hardening(spec.backend) {
        return out;
    }
    if spec.no_new_privileges {
        out.push("no-new-privileges");
    }
    if spec.pids_limit.is_some() {
        out.push("pids-limit");
    }
    out
}

/// `--cpus` as `backend` will accept it.
///
/// docker/podman take a fractional core count (`"1.5"`); Apple's `container`
/// parses an **integer** and rejects anything else. Round up so a fractional cap
/// never silently becomes a tighter one than the user asked for, and floor at 1
/// so a sub-core request doesn't become `--cpus 0`.
pub(crate) fn cpu_limit_for(backend: Backend, cpus: &str) -> String {
    if backend != Backend::Apple {
        return cpus.to_string();
    }
    match cpus.trim().parse::<f64>() {
        Ok(n) if n.is_finite() && n > 0.0 => (n.ceil() as u64).max(1).to_string(),
        // Unparseable: hand it through untouched rather than invent a number —
        // the runtime's own error names the real problem.
        _ => cpus.to_string(),
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
    oci_prefix_for(spec.backend, spec.oci_host.as_deref())
}

/// [`oci_prefix`] without a full spec: the backend + optional `oci_host` are all
/// that determine the daemon-connection flag. Lets path-only teardowns
/// (`teardown`/`teardown_by_path`) target the same remote daemon a create used.
pub(crate) fn oci_prefix_for(backend: Backend, oci_host: Option<&str>) -> Vec<String> {
    let mut v = backend_prefix(backend);
    let Some(host) = oci_host.map(str::trim).filter(|h| !h.is_empty()) else {
        return v;
    };
    if !backend.is_oci() {
        return v;
    }
    // Insert the global connection flag right after the binary (before the
    // subcommand). For rootful podman the binary is at index 2 (`sudo -n podman`).
    let bin_idx = if backend == Backend::PodmanRootful {
        2
    } else {
        0
    };
    let flags: Vec<String> = match backend {
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
    // Bounded by PROBE_TIMEOUT: these are control-plane probes (`ps`, `stats`)
    // on the recurring container-refresh cadence — a wedged runtime (stuck
    // podman machine, broken overlay) must fail the probe fast, not hang the
    // hydrate thread forever (the raw `Command::output()` this replaced had no
    // deadline).
    let mut argv: Vec<String> = prefix.to_vec();
    argv.extend(args.iter().map(|a| a.to_string()));
    let (ok, stdout) = output_with_timeout(&argv, PROBE_TIMEOUT)?;
    ok.then_some(stdout)
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
    // Build the argv from oci_prefix (so rootful/`oci_host` specs hit the daemon
    // that actually holds the container — a bare `binary()` queries the local
    // rootless store, always empty for those) and wrap it through the
    // placement's control primitive (ssh/kubectl/provider) for remote worktrees.
    // format: CPUPerc|MemUsage
    let mut argv = oci_prefix(spec);
    argv.extend([
        "stats".into(),
        "--no-stream".into(),
        "--format".into(),
        "{{.CPUPerc}}|{{.MemUsage}}".into(),
        spec.name.clone(),
    ]);
    // Bounded like every other control-plane probe: a wedged runtime must not
    // hang the caller.
    let (_, stdout) = output_control_owned(spec, &argv, PROBE_TIMEOUT)?;
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
    // (`container_name_with_profile`), and the `-tgagent` / `-tgvpn` companions of
    // either. Reconcile by reverse-mapping each candidate back to a worktree slug
    // rather than allow-listing exact names — the old allow-list only knew the
    // plain + `-tgagent` forms, so a session launched with a non-default profile
    // or a VPN sidecar was misread as an orphan and force-removed while live.
    // Reaping is fail-closed: any container that maps to an active worktree by any
    // of these forms is kept.
    let active_slugs: Vec<String> = active_worktrees.iter().map(|w| util::slugify(w)).collect();

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

/// The "list every container, including stopped" command for `backend`, appended
/// to [`backend_prefix`]. `None` ⇒ this backend is not swept.
///
/// Apple's CLI is not docker-compatible here: it has no `ps`, and no Go
/// templates. It does have `ls --format json`, whose entries carry a top-level
/// `id` — and since `container run --name X` "uses the specified name as the
/// container ID", thegn's existing `thegn-{slug}` scheme lands in that field
/// unchanged.
///
/// `wsl` is deliberately absent: its command shape is unverified, and this feeds
/// a **force-remove**. A wrong guess here deletes someone's container, so it
/// waits for someone who can check it against the real runtime.
pub(crate) fn gc_list_argv(backend: Backend) -> Option<Vec<&'static str>> {
    match backend {
        Backend::Podman | Backend::PodmanRootful | Backend::Docker | Backend::Smol => {
            Some(vec!["ps", "-a", "--format", "{{.Names}}"])
        }
        Backend::Apple => Some(vec!["ls", "-a", "--format", "json"]),
        _ => None,
    }
}

/// Container names from `backend`'s list output. Pure, so both shapes are tested
/// without a runtime. Unparseable output yields nothing — the sweep force-removes
/// what this returns, so "I don't understand this" must mean "delete nothing",
/// never "delete everything".
pub(crate) fn parse_container_list(backend: Backend, stdout: &str) -> Vec<String> {
    match backend {
        Backend::Apple => serde_json::from_str::<serde_json::Value>(stdout)
            .ok()
            .and_then(|v| v.as_array().cloned())
            .unwrap_or_default()
            .iter()
            .filter_map(|e| e.get("id")?.as_str().map(str::to_string))
            .collect(),
        _ => stdout
            .lines()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
    }
}

/// Remove orphaned thegn containers (containers whose worktree no longer
/// exists in the DB). Returns the names of containers that were removed.
///
/// Sweeps every OCI backend that could hold a thegn container, not a fixed list:
/// the old `[Podman, Docker, Smol]` silently leaked **rootful podman** (a
/// separate container store, so the rootless pass never sees it) on Linux and
/// **apple** on macOS — where each leaked container also pins its own Linux VM.
pub fn run_gc(db_worktrees: &[String]) -> Vec<String> {
    run_gc_detailed(db_worktrees)
        .into_iter()
        .flat_map(|(_, v)| v)
        .collect()
}

/// [`run_gc`] with the removals grouped by backend, for the on-demand
/// `thegn sandbox gc` report ("removed N on `<backend>`"). The startup sweep
/// uses the flattened [`run_gc`]; both share this loop.
pub fn run_gc_detailed(db_worktrees: &[String]) -> Vec<(&'static str, Vec<String>)> {
    let mut out = Vec::new();
    for backend in Backend::all_oci() {
        let Some(list) = gc_list_argv(backend) else {
            continue;
        };
        // `available` (not a bare PATH check) so a backend whose daemon is down
        // is skipped rather than probed — we could not list it anyway — and so
        // `sudo -n podman` only runs where rootful is actually usable. Rides the
        // probe cache, so this costs nothing after the resolver's first pass.
        if available(&Placement::Local, backend) != RuntimeProbe::Present {
            continue;
        }

        // Bounded like every other control-plane call — this runs on the startup
        // spawn_blocking task, so a wedged runtime must not pin it forever.
        let mut ps = backend_prefix(backend);
        ps.extend(list.iter().map(|s| (*s).to_string()));
        let Some((ok, stdout)) = output_with_timeout(&ps, PROBE_TIMEOUT) else {
            continue;
        };
        if !ok {
            continue;
        }

        let containers = parse_container_list(backend, &stdout);

        let mut removed = Vec::new();
        for orphan in identify_orphans(db_worktrees, &containers) {
            let mut rm = backend_prefix(backend);
            rm.extend(["rm".into(), "-f".into(), orphan.clone()]);
            let _ = status_with_timeout(&rm, PROBE_TIMEOUT);
            removed.push(orphan);
        }
        if !removed.is_empty() {
            out.push((backend.label(), removed));
        }
    }
    out
}

/// Which owned resource kinds a prune touches. Default = all three.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PruneKinds {
    pub containers: bool,
    pub images: bool,
    pub volumes: bool,
}

impl PruneKinds {
    /// All three kinds — the default when no per-kind flag is given.
    pub fn all() -> PruneKinds {
        PruneKinds {
            containers: true,
            images: true,
            volumes: true,
        }
    }
}

/// What a prune affected (removed when `execute`, else the would-remove listing).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PruneReport {
    /// Owned + stopped container names.
    pub containers: Vec<String>,
    /// Owned image references.
    pub images: Vec<String>,
    /// Prunable owned volume names.
    pub volumes: Vec<String>,
    /// Persistent-role volumes SKIPPED (named, so the user sees what was kept).
    pub kept_volumes: Vec<String>,
    /// Reclaimable bytes across images + volumes, where the engine reported size.
    pub bytes: u64,
}

impl PruneReport {
    pub fn is_empty(&self) -> bool {
        self.containers.is_empty() && self.images.is_empty() && self.volumes.is_empty()
    }
}

/// Prune thegn-owned, stopped containers and `thegn.managed` images/volumes
/// across the detected docker/podman backends. Owned-only by construction: the
/// removal argv is built from an ownership witness (`sandbox_manage`), so a
/// foreign resource is never targeted; persistent-role volumes are skipped and
/// named. When `execute` is false this only lists candidates (the dry-run /
/// pre-confirm plan); when true it removes each and returns what it removed.
pub fn prune_local(kinds: PruneKinds, execute: bool) -> PruneReport {
    use crate::sandbox_manage as m;
    let mut rep = PruneReport::default();
    let run = |prefix: &[String], argv: &[String]| -> Option<String> {
        let a: Vec<&str> = argv.iter().map(String::as_str).collect();
        run_local_output(prefix, &a)
    };
    let rm = |prefix: &[String], sub: Vec<String>| {
        let mut a = prefix.to_vec();
        a.extend(sub);
        let _ = status_with_timeout(&a, PROBE_TIMEOUT);
    };
    for backend in [Backend::Podman, Backend::PodmanRootful, Backend::Docker] {
        if available(&Placement::Local, backend) != RuntimeProbe::Present {
            continue;
        }
        let prefix = backend_prefix(backend);
        // Containers first, so their images/volumes are free to remove next.
        if kinds.containers
            && let Some(argv) = m::mgmt_list_argv(backend)
            && let Some(out) = run(&prefix, &argv)
        {
            let rows = if backend == Backend::Docker {
                parse_docker_ps(&out)
            } else {
                parse_podman_ps(&out)
            };
            for c in rows
                .iter()
                .filter(|c| c.ours && !m::container_running(&c.status))
            {
                if let Some(owned) = m::OwnedContainer::claim(&c.name) {
                    if execute
                        && let Some(sub) =
                            m::mgmt_control_argv(backend, m::ControlOp::Remove, &owned)
                    {
                        rm(&prefix, sub);
                    }
                    rep.containers.push(c.name.clone());
                }
            }
        }
        if kinds.images
            && let Some(argv) = m::mgmt_image_list_argv(backend)
            && let Some(out) = run(&prefix, &argv)
        {
            for img in m::parse_owned_images(&out) {
                if execute && let Some(sub) = m::mgmt_image_rm_argv(backend, &img) {
                    rm(&prefix, sub);
                }
                rep.bytes += img.size_bytes.unwrap_or(0);
                rep.images.push(img.reference);
            }
        }
        if kinds.volumes
            && let Some(argv) = m::mgmt_volume_list_argv(backend)
            && let Some(out) = run(&prefix, &argv)
        {
            for vol in m::parse_owned_volumes(&out) {
                if vol.is_persistent() {
                    rep.kept_volumes.push(vol.name().to_string());
                    continue;
                }
                if execute && let Some(sub) = m::mgmt_volume_rm_argv(backend, &vol) {
                    rm(&prefix, sub);
                }
                rep.bytes += vol.size_bytes.unwrap_or(0);
                rep.volumes.push(vol.name().to_string());
            }
        }
    }
    rep
}

#[cfg(test)]
#[path = "sandbox_tests.rs"]
mod tests;
