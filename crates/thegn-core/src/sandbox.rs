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
use crate::sandbox_events::EventsCap;
use crate::sandbox_mounts::{
    auto_cache_mounts, default_writable_carveouts, host_toolchain_mounts_ro_home, keep_cfg_mount,
    parse_mount_checked,
};
use crate::{msg, util};
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, SystemTime};

#[path = "sandbox_proc.rs"]
pub(crate) mod proc;

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
    proc::run_bounded(
        argv,
        timeout,
        proc::Capture {
            stdout: Some(256 * 1024),
            stderr: None,
        },
    )
    .into_stdout()
}

/// Run `argv` and return its **stderr**, whether or not it succeeded.
///
/// [`output_with_timeout`] pipes stdout and sends stderr to `/dev/null`, which is
/// right for the control-plane probes it serves (they are parsed for their
/// output, and a failure is just a `false`). Container *create* is the one call
/// where the diagnosis lives entirely in stderr: `podman run` exits 125 with a
/// single line naming the bind it refused, and discarding it is what left users
/// with a bare `could not start podman container '<name>'`.
///
/// Returns `None` only if the process could not be spawned or hit the deadline —
/// the same three-state shape as its sibling, so a timeout is never mistaken for
/// a clean run.
pub(crate) fn stderr_with_timeout(argv: &[String], timeout: Duration) -> Option<(bool, String)> {
    proc::run_bounded(
        argv,
        timeout,
        proc::Capture {
            stdout: None,
            stderr: Some(256 * 1024),
        },
    )
    .into_stderr()
}

/// Run an OCI command while retaining bounded stdout and stderr for fail-closed probes.
pub(crate) fn output_and_stderr_with_timeout(
    argv: &[String],
    timeout: Duration,
) -> Option<(bool, String, String)> {
    proc::run_bounded(
        argv,
        timeout,
        proc::Capture {
            stdout: Some(256 * 1024),
            stderr: Some(256 * 1024),
        },
    )
    .into_stdout_and_stderr()
}

/// Run a control command while delivering a bounded secret payload on stdin.
/// The feeder owns only the pipe; this function retains ownership of the child
/// and kills and waits for it on write failure or timeout.
fn stderr_with_timeout_stdin(
    argv: &[String],
    timeout: Duration,
    input: Vec<u8>,
) -> Option<(bool, String)> {
    let redactions: Vec<String> = String::from_utf8_lossy(&input)
        .lines()
        .filter_map(|line| line.split_once('=').map(|(_, value)| value.to_string()))
        .filter(|value| !value.is_empty())
        .collect();
    let (outcome, delivery) = proc::run_bounded_with_input(
        argv,
        timeout,
        proc::Capture {
            stdout: None,
            stderr: Some(256 * 1024),
        },
        input,
    );
    outcome.into_stderr().map(|(success, stderr)| {
        (
            success && delivery == proc::InputDelivery::Complete,
            redact_payload_stderr(stderr, &redactions),
        )
    })
}

#[cfg(test)]
fn redact_stderr(mut stderr: String, secrets: &[String]) -> String {
    for secret in secrets {
        if secret.len() >= 8 {
            stderr = stderr.replace(secret, "[REDACTED]");
        }
    }
    stderr
}

/// Redact values delivered through the secret stdin payload, including short
/// values that the general diagnostic redactor treats as too common to mask.
fn redact_payload_stderr(mut stderr: String, secrets: &[String]) -> String {
    for secret in secrets.iter().filter(|secret| !secret.is_empty()) {
        stderr = stderr.replace(secret.as_str(), "[REDACTED]");
    }
    stderr
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
                events: EventsCap::Yes,
            },
            Backend::PodmanRootful => &BackendProfile {
                label: "podman-rootful",
                binary: "podman",
                family: BackendFamily::Oci,
                rootful: true,
                events: EventsCap::Yes,
            },
            Backend::Docker => &BackendProfile {
                label: "docker",
                binary: "docker",
                family: BackendFamily::Oci,
                rootful: true,
                events: EventsCap::Reserved(
                    "docker has a daemon event stream but its JSON schema differs — not implemented",
                ),
            },
            Backend::Smol => &BackendProfile {
                label: "smolmachines",
                binary: "smolmachines",
                family: BackendFamily::Oci,
                rootful: false,
                events: EventsCap::Reserved("unverified runtime — see Backend::verified"),
            },
            Backend::Bwrap => &BackendProfile {
                label: "bwrap",
                binary: "bwrap",
                family: BackendFamily::Bwrap,
                rootful: false,
                events: EventsCap::No,
            },
            Backend::Systemd => &BackendProfile {
                label: "systemd",
                binary: "systemd-run",
                family: BackendFamily::Systemd,
                rootful: false,
                events: EventsCap::No,
            },
            Backend::Apple => &BackendProfile {
                label: "apple",
                binary: "container",
                family: BackendFamily::Oci,
                rootful: false,
                events: EventsCap::Reserved(
                    "the apple `container` runtime has no podman-compatible events CLI",
                ),
            },
            Backend::Wsl => &BackendProfile {
                label: "wsl",
                binary: "wsl.exe",
                family: BackendFamily::Oci,
                rootful: false,
                events: EventsCap::Reserved("reserved kind — no runtime behind it yet"),
            },
            Backend::WinAppContainer => &BackendProfile {
                label: "appcontainer",
                binary: "",
                family: BackendFamily::WinAppContainer,
                rootful: false,
                events: EventsCap::No,
            },
            Backend::WinJobObject => &BackendProfile {
                label: "jobobject",
                binary: "",
                family: BackendFamily::WinJobObject,
                rootful: false,
                events: EventsCap::No,
            },
            Backend::None => &BackendProfile {
                label: "host",
                binary: "",
                family: BackendFamily::Host,
                rootful: false,
                events: EventsCap::No,
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
    /// The private `liveness_argv` returns `None` for both, so they fall back to a
    /// bare
    /// PATH probe: **"the binary exists" stands in for "the runtime works"**,
    /// which is exactly the defect `06ec12ff` fixed for docker and Apple, where a
    /// stopped daemon was selected and then failed every pane.
    ///
    /// Neither is in `config_defaults::default_backend_chain`, so
    /// nothing reaches them by accident — an unverified backend is only ever
    /// something a user asked for by name, and this is what lets thegn say so
    /// instead of implying a guarantee it has not earned.
    ///
    /// The honest fix is to verify the verbs against a real install, not to
    /// invent them: guessing is how the Apple backend ended up emitting
    /// `container pull` and `container image exists`, neither of which exists.
    /// When someone does that, flip this and add a `liveness_argv` arm.
    ///
    /// `winappcontainer` is also unverified: it is reserved, has no token, ACL,
    /// or network implementation, and currently launches a plain host shell.
    pub fn verified(self) -> bool {
        !matches!(
            self,
            Backend::Smol | Backend::Wsl | Backend::WinAppContainer
        )
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
    /// The optional container-events op of the seam (`sandbox_events`): the
    /// per-backend implemented-or-reserved decision, like every other column
    /// here — never re-derived by a `match backend`.
    pub events: EventsCap,
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
    /// `Some(home)` when the profile demands a **confidential** `$HOME` (sealed
    /// tiers, THE-215), holding the host `$HOME` path to hide: it is replaced by a private tmpfs holding only an explicit
    /// read-only allowlist ([`crate::sandbox_mounts::sealed_home_allowlist`]),
    /// never mounted wholesale. A backend that cannot honor this is refused by
    /// [`crate::sandbox_floor::home_gate`] rather than silently exposing `$HOME`.
    pub seal_home: Option<String>,
    /// Named volumes bound to a devcontainer owner id; empty for every non-devcontainer launch.
    pub volume_owners: Vec<(String, String)>,
}

/// Why a programmatic/configured named volume was refused. The error carries
/// only bounded metadata: it never retains or prints the source string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VolumeNameReason {
    Empty,
    TooShort,
    InvalidFirstByte,
    InvalidByte,
}

impl std::fmt::Display for VolumeNameReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let reason = match self {
            Self::Empty => "empty",
            Self::TooShort => "minimum-two",
            Self::InvalidFirstByte => "first-byte",
            Self::InvalidByte => "character",
        };
        f.write_str(reason)
    }
}

/// A bounded, redacted refusal for one `SandboxSpec.volumes` pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VolumeAdmissionError {
    pub pair_index: usize,
    pub name_len: usize,
    pub reason: VolumeNameReason,
}

impl std::fmt::Display for VolumeAdmissionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "sandbox volume refused (pair {}; {} bytes; reason {})",
            self.pair_index, self.name_len, self.reason
        )
    }
}

impl std::error::Error for VolumeAdmissionError {}

/// Policy category for a sandbox launch refusal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefusalClass {
    ComposePolicy,
    SealedHome,
    UnsafeMountPath,
    SystemdMountPath,
    ComposeTarget,
    ComposeServiceMissing,
    HostnamePolicy,
    /// A local mount source could not be resolved to a stable host path.
    MountUnresolvable,
    /// A named devcontainer volume belongs to another devcontainer identity.
    VolumeOwner,
    /// A tunnel-only profile was requested but thegn cannot enforce that the tunnel is the only route.
    TunnelUnenforced,
    /// A mounted bind source no longer matches the identity admitted before create.
    MountDrift,
    /// A sandbox bind would expose the daemon's privileged control socket.
    ControlSocketReachable,
    /// A sealed profile requested a control this backend cannot enforce.
    StrictUnsupported,
}

impl RefusalClass {
    /// Stable machine-readable label for this refusal category.
    pub fn label(self) -> &'static str {
        match self {
            Self::ComposePolicy => "compose-policy",
            Self::SealedHome => "sealed-home",
            Self::UnsafeMountPath => "unsafe-mount-path",
            Self::SystemdMountPath => "systemd-mount-path",
            Self::ComposeTarget => "compose-target",
            Self::ComposeServiceMissing => "compose-service-missing",
            Self::HostnamePolicy => "hostname-policy",
            Self::MountUnresolvable => "mount-unresolvable",
            Self::VolumeOwner => "volume-owner",
            Self::TunnelUnenforced => "tunnel-unenforced",
            Self::MountDrift => "mount-drift",
            Self::ControlSocketReachable => "control-socket-reachable",
            Self::StrictUnsupported => "strict-unsupported",
        }
    }

    /// One-line user action that can resolve this refusal.
    pub fn remediation(self) -> &'static str {
        match self {
            Self::ComposePolicy => {
                "remove the protected host path from the Compose file's binds, or choose a less restrictive [sandbox] profile"
            }
            Self::SealedHome => {
                "sealed profiles hide $HOME: move the mount or worktree out of $HOME, or use profile 'standard'"
            }
            Self::UnsafeMountPath => {
                "rename the mount path so it has no whitespace, quote, backslash or ':', or use the bwrap backend"
            }
            Self::SystemdMountPath => {
                "rename the mount path so systemd-run can express it, or use the bwrap backend"
            }
            Self::ComposeTarget => {
                "Compose needs the docker or podman backend: set [sandbox] backend to one of them"
            }
            Self::ComposeServiceMissing => {
                "name the service to enter (`service` in the devcontainer or [sandbox] Compose config)"
            }
            Self::HostnamePolicy => {
                "remove the hostname override, or choose a profile that allows it"
            }
            Self::MountUnresolvable => {
                "create the mount source or remove the mount from the config, then relaunch"
            }
            Self::VolumeOwner => {
                "a named volume belongs to another devcontainer: rename the volume in devcontainer.json, \
                 or remove the other volume after checking its contents"
            }
            Self::TunnelUnenforced => {
                "use profile 'standard' with [sandbox.vpn], or 'sealed' for no network"
            }
            Self::MountDrift => {
                "a mount source changed during launch (check for a symlink swap), then relaunch"
            }
            Self::ControlSocketReachable => {
                "move [daemon] socket and $XDG_RUNTIME_DIR out of $HOME, the worktree and every sandbox mount (the default /run/user/<uid> is never mounted), or use the bwrap backend, which masks it"
            }
            Self::StrictUnsupported => {
                "use a backend that enforces every sealed control (bwrap or an OCI runtime), or choose a non-sealed profile"
            }
        }
    }
}

/// Structured, safe context for a refused sandbox launch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub class: RefusalClass,
    pub backend: String,
    pub path: Option<String>,
    pub message: String,
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// Why [`enter_argv`] refused to build a launch command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnterError {
    /// A named volume failed admission.
    Volume(VolumeAdmissionError),
    /// A policy or backend-specific launch refusal.
    Refused(Refusal),
}

impl std::fmt::Display for EnterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EnterError::Volume(e) => e.fmt(f),
            EnterError::Refused(refusal) => refusal.fmt(f),
        }
    }
}

impl EnterError {
    /// Render a stable refusal category, safe path context, and next action.
    pub fn remediation_hint(&self) -> String {
        match self {
            Self::Refused(refusal) => format!(
                "sandbox entry refused [{}]{}: {}",
                refusal.class.label(),
                match refusal.path.as_deref() {
                    Some(path) => format!(" at `{}`", path.escape_debug()),
                    None => String::new(),
                },
                refusal.class.remediation()
            ),
            Self::Volume(_) => "sandbox entry refused [volume-name]: rename the named volume to a plain name (letters, digits, '.', '_', '-')".into(),
        }
    }
}

impl std::error::Error for EnterError {}

impl From<VolumeAdmissionError> for EnterError {
    fn from(e: VolumeAdmissionError) -> Self {
        EnterError::Volume(e)
    }
}

pub(crate) fn refusal(
    spec: &SandboxSpec,
    class: RefusalClass,
    path: Option<String>,
    message: String,
) -> EnterError {
    EnterError::Refused(Refusal {
        class,
        backend: spec.backend.label().to_string(),
        path,
        message,
    })
}

fn resolve_mount_source(host: &std::path::Path) -> std::io::Result<PathBuf> {
    if !host.is_absolute() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "local mount source must be absolute",
        ));
    }
    match host.canonicalize() {
        Ok(source) => Ok(source),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            match std::fs::symlink_metadata(host) {
                Err(metadata_error) if metadata_error.kind() == std::io::ErrorKind::NotFound => {}
                Err(metadata_error) => return Err(metadata_error),
                Ok(_) => return Err(error),
            }

            let mut ancestor = host.to_path_buf();
            let mut missing = Vec::new();
            loop {
                match std::fs::symlink_metadata(&ancestor) {
                    Ok(_) => {
                        let mut resolved = ancestor.canonicalize()?;
                        for component in missing.iter().rev() {
                            resolved.push(component);
                        }
                        return Ok(resolved);
                    }
                    Err(metadata_error)
                        if metadata_error.kind() == std::io::ErrorKind::NotFound =>
                    {
                        let Some(name) = ancestor.file_name() else {
                            return Err(error);
                        };
                        missing.push(name.to_os_string());
                        let Some(parent) = ancestor.parent() else {
                            return Err(error);
                        };
                        ancestor = parent.to_path_buf();
                    }
                    Err(metadata_error) => return Err(metadata_error),
                }
            }
        }
        Err(error) => Err(error),
    }
}

/// The spec as it will actually be launched. For a sealed spec this drops every
/// mount whose host path or destination lies inside the credential deny list
/// (or whose allowlist target was retargeted since resolution) — whoever added
/// it and whenever: bundle identities, agent provider homes, cache injectors all
/// run AFTER the resolver, so this is the single chokepoint every launcher goes
/// through. The managed per-account dirs are the only named exception.
pub fn sealed_view(spec: &SandboxSpec) -> std::borrow::Cow<'_, SandboxSpec> {
    let Some(home) = spec.seal_home.as_deref().filter(|h| !h.is_empty()) else {
        return std::borrow::Cow::Borrowed(spec);
    };
    let home = std::path::Path::new(home);
    // Warn once per path per process, and never for the built-in agent-config
    // carve (`~/.claude`/`~/.codex`), which a sealed launch always drops.
    let ok = |m: &Mount| {
        let keep = crate::sandbox_mounts::sealed_mount_still_ok(m, home);
        if !keep && !crate::sandbox_mounts::is_builtin_agent_carve(&m.host) {
            warn_dropped_mount_once(
                &m.host,
                &m.host,
                "sealed profile mount reaches a protected $HOME path",
            );
        }
        keep
    };
    if spec
        .mounts
        .iter()
        .all(|m| crate::sandbox_mounts::sealed_mount_still_ok(m, home))
    {
        return std::borrow::Cow::Borrowed(spec);
    }
    let mut v = spec.clone();
    v.mounts.retain(|m| ok(m));
    std::borrow::Cow::Owned(v)
}

/// Every launch gate in one place: volume admission, the sealed deny filter and
/// `$HOME` gate, and the OCI `-v` path syntax check.
fn launchable(spec: &SandboxSpec) -> Result<std::borrow::Cow<'_, SandboxSpec>, EnterError> {
    admit_volumes(spec)?;
    if let Some(message) = compose_policy_refusal(spec) {
        return Err(refusal(spec, RefusalClass::ComposePolicy, None, message));
    }
    if let Some(message) = hostname_policy_refusal_with(HOSTNAME_RULES, spec) {
        return Err(refusal(spec, RefusalClass::HostnamePolicy, None, message));
    }
    if let Some(message) = crate::sandbox_floor::strict_gate(spec) {
        return Err(refusal(
            spec,
            RefusalClass::StrictUnsupported,
            None,
            message,
        ));
    }
    tunnel_admission(spec)?;
    if spec.placement.is_local()
        && spec.backend.is_oci()
        && !matches!(spec.file_access, FileAccess::All | FileAccess::Host)
    {
        let visible_roots: Vec<&std::path::Path> = spec
            .mounts
            .iter()
            .map(|mount| std::path::Path::new(&mount.dest))
            .collect();
        if let Some(exposure) = crate::sandbox_control_guard::exposure(
            &crate::sandbox_control_guard::control_dirs(),
            &visible_roots,
        ) {
            return Err(refusal(
                spec,
                RefusalClass::ControlSocketReachable,
                Some(exposure.control_dir.display().to_string()),
                format!(
                    "daemon control directory {} is visible through sandbox mount {}; OCI mounts are fixed when the container is created",
                    exposure.control_dir.display(),
                    exposure.visible_through.display()
                ),
            ));
        }
    }
    let mut view = if !spec.backend.is_oci() && spec.compose.is_some() {
        let mut owned = spec.clone();
        owned.compose = None;
        std::borrow::Cow::Owned(sealed_view(&owned).into_owned())
    } else {
        sealed_view(spec)
    };
    let mut canonical_view = view.into_owned();
    let mut keep_mount = vec![true; canonical_view.mounts.len()];
    if spec.placement.is_local() {
        for (index, mount) in canonical_view.mounts.iter_mut().enumerate() {
            match resolve_mount_source(std::path::Path::new(&mount.host)) {
                Ok(source) => {
                    // Validate and emit the same host identity while preserving the
                    // path-visible destination used by bwrap, OCI, and systemd.
                    mount.host = source.to_string_lossy().into_owned();
                }
                Err(error) if mount.cache => {
                    keep_mount[index] = false;
                    tracing::warn!(
                        target: "thegn::sandbox",
                        source = %mount.host,
                        dest = %mount.dest,
                        %error,
                        "omitting unresolvable cache mount"
                    );
                }
                Err(error) => {
                    return Err(refusal(
                        spec,
                        RefusalClass::MountUnresolvable,
                        Some(mount.dest.clone()),
                        format!(
                            "sandbox mount source `{}` (destination `{}`) cannot be resolved: {error}; refusing to launch with an unverified mount",
                            mount.host, mount.dest
                        ),
                    ));
                }
            }
        }
    }
    let mut index = 0;
    canonical_view.mounts.retain(|_| {
        let keep = keep_mount[index];
        index += 1;
        keep
    });
    view = std::borrow::Cow::Owned(canonical_view);
    if let Some(m) = crate::sandbox_floor::home_gate(&view) {
        return Err(refusal(
            spec,
            RefusalClass::SealedHome,
            sealed_home_offending_path(&view),
            m,
        ));
    }
    if view.backend.profile().family == BackendFamily::Oci
        && let Some(m) = view.mounts.iter().find(|m| {
            crate::sandbox_mounts::mount_path_unsafe(&m.host)
                || crate::sandbox_mounts::mount_path_unsafe(&m.dest)
        })
    {
        return Err(refusal(
            spec,
            RefusalClass::UnsafeMountPath,
            Some(m.dest.clone()),
            format!(
                "OCI mount `{}` contains whitespace, a quote, a backslash or ':' which a `-v` \
                 spec cannot express safely",
                m.dest
            ),
        ));
    }
    if view.backend.profile().family == BackendFamily::Systemd {
        systemd_mount_props(&view).map_err(|message| {
            refusal(
                spec,
                RefusalClass::SystemdMountPath,
                systemd_unsafe_mount_path(&view),
                message,
            )
        })?;
    }
    Ok(view)
}

/// Refusal for a Compose-backed OCI launch whose requested restrictions Compose
/// cannot apply. Reused by the candidate walk so a miss cannot fall through to
/// a host shell.
pub fn compose_policy_refusal(spec: &SandboxSpec) -> Option<String> {
    (spec.backend.is_oci() && spec.compose_spec().is_some() && compose_cannot_enforce(spec)).then(
        || {
            "Compose cannot enforce the requested sandbox restrictions on OCI backends: \
         read_only_root, no_new_privileges, pids_limit, dropped capabilities, sealed home, \
         network-none or network filters, oci_runtime, per-pane CPU/memory limits, and VPN. \
         Remove the conflicting restriction or Compose config."
                .to_string()
        },
    )
}

fn compose_cannot_enforce(spec: &SandboxSpec) -> bool {
    spec.read_only_root
        || spec.no_new_privileges
        || spec.pids_limit.is_some()
        || !spec.drop_capabilities.is_empty()
        || spec.seal_home.is_some()
        || spec.network == Network::None
        || !spec.network_allow.is_empty()
        || !spec.network_block.is_empty()
        || spec.oci_runtime.is_some()
        || spec.limits.cpu.is_some()
        || spec.limits.memory.is_some()
        || spec.vpn.is_some()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostnameRulesPolicy {
    WarnDnsOnly,
    Refuse,
}

/// SD-3 (THE-799): flip to `Refuse` for the audit design; one line.
pub const HOSTNAME_RULES: HostnameRulesPolicy = HostnameRulesPolicy::WarnDnsOnly;

pub fn lower_universal_deny(network: Network, block: &[String]) -> Network {
    if block.iter().any(|rule| rule.trim() == "*") {
        Network::None
    } else {
        network
    }
}

pub fn hostname_rules_active(network: Network, allow: &[String], block: &[String]) -> bool {
    network != Network::None && (!allow.is_empty() || !block.is_empty())
}

pub fn hostname_rules_note_for(
    network: Network,
    allow: &[String],
    block: &[String],
    backend: Backend,
    placement: &Placement,
) -> Option<String> {
    if !hostname_rules_active(network, allow, block) {
        return None;
    }
    // Provider placements lower these rules to provider egress policy (THE-534).
    if matches!(placement, Placement::Provider(_)) {
        return None;
    }
    Some(format!(
        "sandbox {}: network_allow/network_block are DNS-name rules only — not an egress boundary, and not \
         applied by this backend (direct IPs and other resolvers bypass them); set `network = \"none\"` \
         to deny egress, or remove the lists",
        backend.label()
    ))
}

pub fn hostname_rules_note(spec: &SandboxSpec) -> Option<String> {
    hostname_rules_note_for(
        spec.network,
        &spec.network_allow,
        &spec.network_block,
        spec.backend,
        &spec.placement,
    )
}

fn hostname_policy_refusal_with(policy: HostnameRulesPolicy, spec: &SandboxSpec) -> Option<String> {
    if policy == HostnameRulesPolicy::WarnDnsOnly
        || !hostname_rules_active(spec.network, &spec.network_allow, &spec.network_block)
        || matches!(spec.placement, Placement::Provider(_) | Placement::K8s(_))
    {
        return None;
    }
    Some(format!(
        "`network_allow`/`network_block` are DNS-name rules, which are not an egress boundary and are not \
         enforced by the `{}` backend; set `network = \"none\"` or remove the lists",
        spec.backend.label()
    ))
}

/// The portable Thegn named-volume lexical policy. A source must begin with
/// ASCII alphanumeric, be at least two bytes, and contain only ASCII letters,
/// digits, `_`, `-`, or `.`. This rejects bind/path and option syntax without
/// filesystem access or path normalization.
pub fn validate_volume_name(name: &str) -> Result<(), VolumeAdmissionError> {
    validate_volume_name_at(0, name)
}

fn validate_volume_name_at(pair_index: usize, name: &str) -> Result<(), VolumeAdmissionError> {
    let error = |reason| VolumeAdmissionError {
        pair_index,
        name_len: name.len(),
        reason,
    };
    if name.is_empty() {
        return Err(error(VolumeNameReason::Empty));
    }
    if name.len() < 2 {
        return Err(error(VolumeNameReason::TooShort));
    }
    if !name.as_bytes()[0].is_ascii_alphanumeric() {
        return Err(error(VolumeNameReason::InvalidFirstByte));
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
    {
        return Err(error(VolumeNameReason::InvalidByte));
    }
    Ok(())
}

/// Admit the fully composed spec before any backend argv, secret-file, VPN, or
/// container side effect. `None` remains the separate disabled/host sentinel.
pub fn admit_volumes(spec: &SandboxSpec) -> Result<(), VolumeAdmissionError> {
    admit_volume_sources(spec.volumes.iter().map(|(name, _dest)| name.as_str()))
}

/// Admit configured volume sources before backend resolution. This is kept
/// separate from [`admit_volumes`] because a resolver may return `None`, in
/// which case there is no [`SandboxSpec`] to validate before the caller's host
/// fallback. The caller decides whether the configured sandbox is enabled;
/// an explicit `none`/disabled configuration remains its own host policy.
pub fn admit_volume_sources<'a, I>(sources: I) -> Result<(), VolumeAdmissionError>
where
    I: IntoIterator<Item = &'a str>,
{
    for (pair_index, name) in sources.into_iter().enumerate() {
        validate_volume_name_at(pair_index, name)?;
    }
    Ok(())
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
    /// The profile asked for the tunnel to be the ONLY route (`sealed-tunnel`); thegn cannot enforce
    /// that yet, so launch admission refuses such a spec.
    pub tunnel_only: bool,
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
/// no-caps contract). A `sealed-tunnel` request is recorded as tunnel-only and
/// refused at launch admission until thegn can enforce an exclusive tunnel route.
pub fn build_vpn_spec(cfg: &VpnConfig, name: &str, profile: SandboxProfile) -> Option<VpnSpec> {
    if !cfg.is_enabled() {
        return None;
    }
    if !profile.permits_vpn() {
        msg::warn(&format!(
            "sandbox: profile '{profile}' forbids a VPN tunnel (network=none, all \
             capabilities dropped); ignoring [sandbox.vpn]. Use 'sealed-tunnel' for a \
             tunnel-only worktree, or 'standard'.",
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
        tunnel_only: profile == SandboxProfile::SealedTunnel,
        params,
    })
}

/// Refusal message for a tunnel-only VPN request that cannot be enforced.
pub const TUNNEL_ONLY_REFUSAL: &str = "sandbox profile 'sealed-tunnel' promises the VPN tunnel is the only network route, but thegn cannot enforce a tunnel-only datapath yet (direct egress would remain possible beside the sidecar or proxy); refusing to launch. Use profile 'standard' with [sandbox.vpn] for a tunnel that is not exclusive, or 'sealed' for no network.";

/// Refuse a tunnel-only VPN request until the runtime can enforce that no
/// direct egress route exists beside the VPN tunnel.
pub fn tunnel_admission(spec: &SandboxSpec) -> Result<(), EnterError> {
    if spec.vpn.as_ref().is_some_and(|vpn| vpn.tunnel_only) {
        return Err(refusal(
            spec,
            RefusalClass::TunnelUnenforced,
            None,
            TUNNEL_ONLY_REFUSAL.into(),
        ));
    }
    Ok(())
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

/// Warn once per path when a USER-authored `[sandbox] mounts` entry is dropped.
/// Built-in defaults are listed by `thegn doctor` instead.
fn warn_dropped_mount_once(raw: &str, host: &str, reason: &str) {
    use std::sync::{Mutex, OnceLock};
    if SandboxConfig::default().mounts.iter().any(|d| d == raw) {
        return;
    }
    static SEEN: OnceLock<Mutex<std::collections::HashSet<String>>> = OnceLock::new();
    let seen = SEEN.get_or_init(Default::default);
    let first = seen
        .lock()
        .map(|mut s| s.insert(host.to_string()))
        .unwrap_or(false);
    if first {
        tracing::warn!(
            target: "thegn::sandbox",
            path = %host,
            reason = %reason,
            "dropping [sandbox] mounts entry"
        );
    }
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
    let universal_deny = lower_universal_deny(Network::Nat, &cfg.network_block) == Network::None;

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
    // THE-215: sealed tiers never see the ambient $HOME. They get a private
    // tmpfs $HOME (bwrap `--tmpfs`, systemd `ProtectHome=tmpfs`; OCI simply
    // never mounts it) plus a reviewed read-only allowlist — read-only is
    // integrity isolation, not confidentiality, so it is not enough here.
    //
    // An unset/empty/unresolvable `$HOME` does NOT disable sealing: the spec
    // still carries `seal_home = Some(..)` so `sandbox_floor::home_gate` REFUSES
    // the launch instead of quietly running with an unhidden home.
    let seal_home: Option<String> = profile
        .hides_home()
        .then(|| std::env::var("HOME").unwrap_or_default());
    let host_toolchain = || {
        if seal_home.is_some() {
            crate::sandbox_mounts::host_toolchain_mounts_sealed_home()
        } else {
            host_toolchain_mounts_ro_home(home_ro)
        }
    };

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
                mounts.extend(host_toolchain());
            }
            add_worktree_mounts(&mut mounts);
        }
        FileAccess::WorktreePlusCaches => {
            if inject_host_toolchain {
                mounts.extend(host_toolchain());
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
    // Sealed: `$HOME` is a private tmpfs, so there is no host state to carve
    // back (and `.local/{state,share}` hold the credentials we are hiding).
    if inject_host_toolchain && home_ro && seal_home.is_none() {
        for cv in default_writable_carveouts(profile) {
            if keep_cfg_mount(&mounts, &cv) {
                mounts.push(cv);
            }
        }
    }

    // systemd does not consume the toolchain substrate (it sees the host fs), so
    // its sealed `$HOME` allowlist is added here; `ProtectHome=tmpfs` hides the
    // rest and the argv builder binds these back in.
    if let Some(home) = &seal_home
        && !home.is_empty()
        && backend.profile().family == BackendFamily::Systemd
        && matches!(
            cfg.file_access,
            FileAccess::Worktree | FileAccess::WorktreePlusCaches
        )
    {
        mounts.extend(crate::sandbox_mounts::sealed_home_allowlist(
            std::path::Path::new(home),
        ));
    }

    for m in &cfg.mounts {
        let parsed = match parse_mount_checked(m) {
            Ok(parsed) => parsed,
            Err(error) => {
                warn_dropped_mount_once(m, m, &format!("malformed mount: {error}"));
                continue;
            }
        };
        // THE-215: a sealed launch never honors a mount (host path or dest,
        // canonicalized) that reaches the credential deny list — this covers the
        // configured mount as much as a user entry. Drop and name the path.
        if let Some(home) = &seal_home
            && !home.is_empty()
            && crate::sandbox_mounts::sealed_denies_mount(&parsed, std::path::Path::new(home))
        {
            warn_dropped_mount_once(
                m,
                &parsed.host,
                "sealed profile mount reaches a protected $HOME path",
            );
            continue;
        }
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
    if inject_host_toolchain && placement.is_local() && seal_home.is_none() {
        let mut covered: Vec<String> = mounts.iter().map(|m| m.dest.clone()).collect();
        if backend == Backend::Bwrap {
            covered.extend(BWRAP_SUBSTRATE.iter().map(|s| s.to_string()));
        }
        mounts.extend(crate::ssh_creds::identity_mounts(&covered));
    }

    // THE-66: sealed / sealed-tunnel tiers get a default-deny secret posture —
    // the SSH agent socket is dropped from the passthrough even if the config
    // still names it (a sealed pane with the user's agent contradicts the tier).
    // Explicit `env_passthrough` on those tiers still re-adds it subject to the
    // profile credential firewall; doctor flags it. Hardened/open keep whatever
    // the passthrough names admitted by that firewall.
    let seal_agent = profile.seals_agent_socket();
    let firewall = crate::profile::CredentialFirewall::active();
    let mut env: Vec<(String, String)> = cfg
        .env_passthrough
        .iter()
        .filter(|k| firewall.admits(k))
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
    // fine. `nix_daemon = true` is the only way to enable this trusted host
    // boundary. Host direnv warming and automatic daemon mounting are removed;
    // an in-sandbox environment must either use target-side policy or remain a
    // normal user-approved shell. The socket is a local unix socket, so this
    // is compatible with `network = none`.
    //
    // Withheld across an ABI boundary (see [`guest_shares_host_abi`]): a Mac's
    // nix-daemon serves *darwin* store paths, which a Linux guest cannot execute
    // — and the bind fails the create outright, because podman machine does not
    // share `/nix`. This is the same gate the host-toolchain mounts already use;
    // it reaches here because the socket is injected separately from them.
    let abi_ok = guest_shares_host_abi(backend, crate::sandbox_backend::host_os());
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
    if abi_ok && cfg.nix_daemon {
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
        network: lower_universal_deny(
            if profile.forces_no_network() {
                Network::None
            } else {
                cfg.network
            },
            &cfg.network_block,
        ),
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
        volume_owners: cfg
            .volume_owners
            .iter()
            .map(|(name, owner)| (name.clone(), owner.clone()))
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
            if cfg.vpn.is_enabled()
                && cfg.network == Network::Host
                && !profile.forces_no_network()
                && !universal_deny
            {
                msg::warn(
                    "sandbox: [sandbox] network=host conflicts with a VPN tunnel \
                     (host networking is what the tunnel isolates from); the worktree \
                     will join the tunnel instead of sharing the host network.",
                );
            }
            if universal_deny {
                None
            } else {
                build_vpn_spec(&cfg.vpn, name, profile)
            }
        },
        oci_host: (!cfg.oci_host.trim().is_empty()).then(|| cfg.oci_host.trim().to_string()),
        oci_runtime: (!cfg.oci_runtime.trim().is_empty())
            .then(|| cfg.oci_runtime.trim().to_string()),
        // Default off: the resolver has no idea whether this pane is a
        // daemon-routed center tab or an ephemeral chrome pane. The pane owner
        // (the host's launch-spec builder) flips it on for daemon-backed panes.
        daemon_persistent: false,
        seal_home,
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

/// Container name derived from the config profile used by the create site.
///
/// Config values are trimmed before being passed to
/// [`container_name_with_profile`]; keeping that normalization here lets
/// ownership checks use the same identity as container creation.
pub fn container_name_for_config_profile(worktree: &str, profile: &str) -> String {
    let profile = profile.trim();
    container_name_with_profile(worktree, (!profile.is_empty()).then_some(profile))
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
/// SANDBOXES section. `ours` is a prefix-based candidate until the host stamps
/// it against this profile's known worktrees and observed backends.
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

/// Proven container identities for worktrees known to one thegn profile.
///
/// Listing parsers only mark names with the thegn prefix as candidates. The
/// host uses this set to lower that candidate bit unless both the exact
/// profile-aware name and the backend match a known worktree.
#[derive(Debug, Clone, Default)]
pub struct OwnedContainers {
    identities: Vec<(String, Option<Backend>)>,
}

impl OwnedContainers {
    /// Build identities from worktree paths and their last observed backends.
    /// Backends that cannot list OCI containers do not establish ownership.
    pub fn new<'a>(
        profile: &str,
        worktrees: impl IntoIterator<Item = (&'a str, Option<&'a str>)>,
    ) -> Self {
        let identities = worktrees
            .into_iter()
            .flat_map(|(path, observed)| {
                let backend = observed.and_then(Backend::parse).filter(|backend| {
                    matches!(
                        backend,
                        Backend::Podman | Backend::PodmanRootful | Backend::Docker
                    )
                });
                let base = container_name_for_config_profile(path, profile);
                [
                    (base.clone(), backend),
                    (vpn_sidecar_name(&base), backend),
                    (agent_container_name(&base), backend),
                ]
            })
            .collect();
        Self { identities }
    }

    /// Whether an exact container name is proven on the same parsed OCI backend.
    pub fn owns(&self, name: &str, backend: &str) -> bool {
        let Some(backend) = Backend::parse(backend) else {
            return false;
        };
        self.identities.iter().any(|(known_name, known_backend)| {
            known_name == name && *known_backend == Some(backend)
        })
    }

    /// Lower prefix candidates that are not proven, then keep the listing order.
    pub fn stamp(&self, rows: &mut [ContainerInfo]) {
        for row in rows.iter_mut() {
            row.ours = row.ours && self.owns(&row.name, &row.backend);
        }
        rows.sort_by_key(|row| (!row.ours, row.name.clone()));
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
/// panel section and monitor rows need on their visible demand cadence. The expensive
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
    running_containers_with(
        with_stats,
        crate::sandbox_backend::available_for_listing,
        |backend, args| run_local_output(&backend_prefix(backend), args),
        oci_stats,
    )
}

fn running_containers_with(
    with_stats: bool,
    mut available: impl FnMut(Backend) -> RuntimeProbe,
    mut run: impl FnMut(Backend, &[&str]) -> Option<String>,
    mut stats: impl FnMut(Backend) -> std::collections::HashMap<String, ContainerStat>,
) -> Vec<ContainerInfo> {
    let mut out = Vec::new();
    if available(Backend::Podman) == RuntimeProbe::Present
        && let Some(stdout) = run(Backend::Podman, &["ps", "--format", "json"])
    {
        let mut rows = parse_podman_ps(&stdout);
        if with_stats {
            apply_stats(&mut rows, &stats(Backend::Podman));
        }
        out.extend(rows);
    }
    if available(Backend::PodmanRootful) == RuntimeProbe::Present
        && let Some(stdout) = run(Backend::PodmanRootful, &["ps", "--format", "json"])
    {
        let mut rows = parse_podman_ps(&stdout);
        for r in &mut rows {
            r.backend = "podman-rootful".into();
        }
        if with_stats {
            apply_stats(&mut rows, &stats(Backend::PodmanRootful));
        }
        out.extend(rows);
    }
    if out.is_empty()
        && available(Backend::Docker) == RuntimeProbe::Present
        && let Some(stdout) = run(Backend::Docker, &["ps", "--format", "{{json .}}"])
    {
        let mut rows = parse_docker_ps(&stdout);
        if with_stats {
            apply_stats(&mut rows, &stats(Backend::Docker));
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

fn first_str<'a>(value: &'a serde_json::Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(serde_json::Value::as_str))
}

pub fn parse_stats_rows(output: &str) -> Vec<(String, ContainerStat)> {
    let parse_one = |v: serde_json::Value| -> Option<(String, ContainerStat)> {
        let name = first_str(&v, &["Name", "Names", "name"])?.to_string();
        let cpu = first_str(&v, &["CPUPerc", "CPU", "cpu_percent"])
            .unwrap_or("")
            .to_string();
        let mem = first_str(&v, &["MemUsage", "Mem", "mem_usage"])
            .unwrap_or("")
            .split('/')
            .next()
            .unwrap_or("")
            .trim()
            .to_string();
        let net = first_str(&v, &["NetIO", "net_io"])
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

/// The default containment image, pinned by **digest** rather than the
/// `stable` tag it was resolved from.
///
/// A tag is a mutable pointer the registry owner can retarget at any time, so
/// `debian:stable` meant the default sandbox ran whatever that name resolved to
/// on the day it was pulled — with no record of what that was. A digest is
/// content-addressed: the registry cannot substitute different bytes under it,
/// and `thegn doctor` can state exactly what the containment boundary is made
/// of.
///
/// This is the multi-arch OCI **index** digest (linux/amd64, arm64/v8, arm/v5,
/// arm/v7, 386, ppc64le, riscv64, s390x), not a single platform's manifest, so
/// every supported host still resolves its own architecture.
///
/// Updating it is deliberate: re-resolve the tag and record what moved.
///
/// ```sh
/// # Resolves docker.io/library/debian:stable to its current index digest.
/// skopeo inspect --raw docker://docker.io/library/debian:stable |
///     sha256sum
/// ```
///
/// The tag is kept alongside the digest (`name:tag@sha256:…`, which both
/// Docker and Podman accept) so the reference still says what it IS. The
/// runtime resolves the digest; the tag is documentation.
///
/// Pinned 2026-09-21 from `debian:stable`.
pub const DEFAULT_OCI_IMAGE: &str = "docker.io/library/debian:stable@sha256:2bf225230f05881b6a36d20f484088f408c9a1309edc730c0bf01f15e635d411";

pub(crate) fn effective_image(spec: &SandboxSpec) -> String {
    spec.image
        .clone()
        .unwrap_or_else(|| DEFAULT_OCI_IMAGE.to_string())
}

/// Whether an image reference is content-addressed (`…@sha256:…`) rather than
/// a mutable tag.
///
/// A configured tag stays the user's choice, but it is a choice the rest of
/// the system should be able to see and report, not a silent default.
pub fn image_is_digest_pinned(image: &str) -> bool {
    // Reuse the real reference parser rather than re-deriving digest syntax:
    // one grammar, one place it can be wrong.
    crate::image::ImageRef::parse(image).is_ok_and(|r| r.manifest_list_digest.is_some())
}

/// `--entrypoint` argv that neutralizes whatever ENTRYPOINT an image declares.
///
/// Without this the runtime PREPENDS the image's own ENTRYPOINT to the command
/// we asked for, so `run … <image> sleep infinity` actually executes
/// `<image-entrypoint> sleep infinity`. An image whose entrypoint is hostile —
/// or merely a wrapper that execs something else — therefore chooses what runs
/// inside the containment boundary, with every mount and grant the sandbox was
/// given.
///
/// Naming `sleep` as the entrypoint outright is stronger than clearing it with
/// an empty string: it is unambiguous across Docker and Podman, and the
/// container's PID 1 is then exactly what we named regardless of image
/// metadata.
pub(crate) fn entrypoint_override() -> [String; 2] {
    ["--entrypoint".into(), "sleep".into()]
}

pub fn health_check(spec: &SandboxSpec) -> bool {
    if !spec.backend.is_oci() {
        return true;
    }
    // Judge the same resolved view that ensure used to create the container.
    launchable(spec)
        .map(|view| {
            let (running, reusable) = container_status(&view);
            running && reusable
        })
        .unwrap_or(false)
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

/// Whether `oci_create_opts` adds the devenv `/nix:/nix:ro` bind for this spec.
/// Shared with the sealed-reuse check so the `/nix` tolerance there is exactly
/// the bind the CURRENT spec would itself create, never a leftover.
fn emits_nix_bind(spec: &SandboxSpec) -> bool {
    spec.devenv
        && spec
            .devenv_path
            .as_deref()
            .is_some_and(|p| p.starts_with("/nix"))
        && std::path::Path::new("/nix").exists()
        && guest_shares_host_abi(spec.backend, crate::sandbox_backend::host_os())
}

/// Label stamped at create recording whether the container was built for a
/// sealed (`$HOME`-hiding) spec. The container name is per worktree, NOT per
/// profile, so reuse must compare it or a `hardened` container (ro `$HOME`,
/// `~/.gnupg` rw) would be reused by a later `sealed` launch.
const SEAL_LABEL: &str = "thegn.seal_home";
pub(crate) const POLICY_LABEL: &str = "thegn.policy";

pub(crate) fn expected_policy(spec: &SandboxSpec) -> String {
    let sealed = sealed_view(spec);
    crate::sandbox_policy_fp::policy_fingerprint(&sealed, vpn_sidecar_join(&sealed).is_some())
}

/// Whether a running container's bind sources satisfy the spec. Non-sealed: the
/// required set must be a subset (historic behaviour). Sealed: the live set must
/// EQUAL the required one — an extra bind could be `~/.ssh` — apart from the
/// `/nix` bind `oci_create_opts` adds itself for devenv, and only when the
/// current spec would still add it (`nix_expected`): a container created with
/// devenv on is recreated once devenv is off.
fn mounts_match(
    required: &std::collections::HashSet<&str>,
    active: &std::collections::HashSet<&str>,
    sealed: bool,
    nix_expected: bool,
) -> bool {
    if !required.iter().all(|r| active.contains(r)) {
        return false;
    }
    !sealed
        || active
            .iter()
            .all(|a| required.contains(a) || (nix_expected && *a == "/nix"))
}

/// Judge `inspect` output in thegn's probe format: line 1 `RUNNING` (or empty),
/// line 2 the [`SEAL_LABEL`] value, line 3 the [`POLICY_LABEL`] value (empty
/// when absent), then one bind source per line. Returns `(running, reusable)`.
/// Reuse requires the seal and policy labels to match and the mounts to pass;
/// an absent policy label identifies a legacy container and forces recreation.
fn judge_inspect(
    stdout: &str,
    required: &std::collections::HashSet<&str>,
    sealed: bool,
    nix_expected: bool,
    expected_fp: &str,
) -> (bool, bool) {
    let mut lines = stdout.lines();
    if lines.next() != Some("RUNNING") {
        return (false, false);
    }
    let label = lines.next().unwrap_or("").trim();
    let label_ok = if sealed { label == "1" } else { label != "1" };
    let policy = lines.next().unwrap_or("").trim();
    let active: std::collections::HashSet<&str> = lines.filter(|l| !l.is_empty()).collect();
    (
        true,
        label_ok && policy == expected_fp && mounts_match(required, &active, sealed, nix_expected),
    )
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
#[cfg(test)]
pub(crate) fn parse_apple_inspect(stdout: &str, required: &[&str]) -> (bool, bool) {
    parse_apple_inspect_sealed(stdout, required, false, false)
}

/// Apple has no verified `--label`, so a sealed spec is judged on mounts alone
/// (and, because it never receives host mounts, equality is cheap and strict).
/// A sealed spec also refuses reuse when `configuration.mounts` is absent (as
/// distinct from present and empty): with no label to lean on, an unreadable
/// mount list must not pass as "nothing extra bound".
fn parse_apple_inspect_sealed(
    stdout: &str,
    required: &[&str],
    sealed: bool,
    nix_expected: bool,
) -> (bool, bool) {
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
    let mounts = first
        .get("configuration")
        .and_then(|c| c.get("mounts"))
        .and_then(|m| m.as_array());
    if sealed && mounts.is_none() {
        return (true, false);
    }
    let active: std::collections::HashSet<&str> = mounts
        .map(|arr| {
            arr.iter()
                .filter_map(|m| m.get("source")?.as_str())
                .collect()
        })
        .unwrap_or_default();
    let req: std::collections::HashSet<&str> = required.iter().copied().collect();
    (true, mounts_match(&req, &active, sealed, nix_expected))
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

/// Return `(running, reusable)` after checking the policy label, mounts, and
/// the runtime image id against the current local image id. Docker reports
/// both ids as `sha256:<64 hex>`; Podman reports both as bare 64-hex ids.
/// Apple retains its mount-only check because its CLI has no verified Go
/// templates or image-id probe (THE-513 residual).
fn container_status(spec: &SandboxSpec) -> (bool, bool) {
    let sealed = spec.seal_home.is_some();
    let nix_expected = emits_nix_bind(spec);
    let expected_fp = expected_policy(spec);
    let required: std::collections::HashSet<&str> = spec
        .mounts
        .iter()
        .filter(|m| oci_emits_mount(m))
        .map(|m| m.host.as_str())
        .collect();

    // Emit "RUNNING" if actually running (not "created"/"exited"), then one
    // bind-mount source per line. A container in "created" state passes inspect
    // but cannot accept exec sessions — we must not treat it as healthy.
    let fmt = "{{if .State.Running}}RUNNING{{end}}\n{{index .Config.Labels \"thegn.seal_home\"}}\n{{index .Config.Labels \"thegn.policy\"}}\n{{range .Mounts}}{{if eq .Type \"bind\"}}{{.Source}}\n{{end}}{{end}}thegn-image:{{.Image}}\n";
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
            // Apple has no verified policy label; mount-only reuse is a THE-513 residual.
            return parse_apple_inspect_sealed(&stdout, &req, sealed, nix_expected);
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
        // First line must be "RUNNING" — "CREATED" / "EXITED" / missing → not usable.
        let (inspect_stdout, container_image) = split_image_line(&stdout);
        let verdict = judge_inspect(
            &inspect_stdout,
            &required,
            sealed,
            nix_expected,
            &expected_fp,
        );
        if verdict != (true, true) {
            return verdict;
        }
        let image_argv = image_id_argv(spec);
        match output_with_timeout(&image_argv, PROBE_TIMEOUT) {
            Some((true, expected_image)) => (
                true,
                image_reusable(container_image.as_deref(), Some(expected_image.as_str())),
            ),
            Some((false, _)) | None => (true, false),
        }
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
        let (inspect_stdout, container_image) = split_image_line(&stdout);
        let verdict = judge_inspect(
            &inspect_stdout,
            &required,
            sealed,
            nix_expected,
            &expected_fp,
        );
        if verdict != (true, true) {
            return verdict;
        }
        let image_argv = image_id_argv(spec);
        match output_control_owned(spec, &image_argv, PROBE_TIMEOUT) {
            Some((true, expected_image)) => (
                true,
                image_reusable(container_image.as_deref(), Some(expected_image.as_str())),
            ),
            Some((false, _)) | None => (true, false),
        }
    }
}

/// Build the runtime probe for the local id of the spec's effective image.
fn image_id_argv(spec: &SandboxSpec) -> Vec<String> {
    let mut argv = oci_prefix(spec);
    argv.extend([
        "image".into(),
        "inspect".into(),
        "--format".into(),
        "{{.Id}}".into(),
        effective_image(spec),
    ]);
    argv
}

/// Docker and Podman expose image ids with and without the `sha256:` prefix.
/// Empty, placeholder, abbreviated, and non-hex values are deliberately
/// unparseable so an uncertain runtime identity can never authorize reuse.
fn normalize_image_id(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    let id = trimmed.strip_prefix("sha256:").unwrap_or(trimmed);
    if id.len() != 64 || !id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    Some(id.to_ascii_lowercase())
}

/// Reuse is allowed only when both runtime-reported ids normalize and match.
fn image_reusable(container: Option<&str>, expected: Option<&str>) -> bool {
    match (
        container.and_then(normalize_image_id),
        expected.and_then(normalize_image_id),
    ) {
        (Some(container), Some(expected)) => container == expected,
        _ => false,
    }
}

/// Remove the trailing image-id line before passing inspect output to
/// `judge_inspect`, whose remaining lines retain their established positions.
/// Only the final line is metadata: a bind source named `thegn-image:x` earlier
/// in the output remains an ordinary mount source.
fn split_image_line(stdout: &str) -> (String, Option<String>) {
    let without_final_newline = stdout.strip_suffix('\n').unwrap_or(stdout);
    let Some((rest, final_line)) = without_final_newline.rsplit_once('\n') else {
        return (stdout.to_string(), None);
    };
    let Some(image_id) = final_line.strip_prefix("thegn-image:") else {
        return (stdout.to_string(), None);
    };
    (
        format!("{rest}\n"),
        Some(image_id.trim_end_matches('\r').to_string()),
    )
}

/// Ensure any persistent state exists (OCI: a keep-alive container we `exec`
/// into). No-op for host-toolchain backends and `none`.
pub fn ensure(spec: &SandboxSpec) -> anyhow::Result<()> {
    let report = ensure_attested(spec, &crate::sandbox_compose::ComposeGuard::for_host(&[]))?;
    if let Some(reason) = report.compose_unverified {
        msg::warn(&format!(
            "Compose config could not be inspected ({reason}); protected host binds were not checked; isolation reported as host-process"
        ));
    }
    Ok(())
}

/// Ensure sandbox state and report whether Compose's effective config was checked.
pub fn ensure_attested(
    spec: &SandboxSpec,
    guard: &crate::sandbox_compose::ComposeGuard,
) -> anyhow::Result<crate::sandbox_compose::EnsureReport> {
    let launchable = &*launchable(spec).map_err(anyhow::Error::new)?;
    if let Some(compose) = launchable.compose_spec() {
        return crate::sandbox_compose::up_and_verify(launchable, &compose, guard);
    }
    ensure_noncompose(launchable)?;
    Ok(crate::sandbox_compose::EnsureReport::default())
}

fn ensure_noncompose(spec: &SandboxSpec) -> anyhow::Result<()> {
    if !spec.backend.is_oci() {
        return Ok(());
    }

    fn post_create_check(
        spec: &SandboxSpec,
        admitted: &[(String, crate::sandbox_mountcheck::MountIdentity)],
    ) -> Result<bool, anyhow::Error> {
        use crate::progress::{SandboxPhase, emit};

        let (running, reusable) = container_status(spec);
        match crate::sandbox_mountcheck::post_create_verdict(
            running,
            reusable,
            crate::sandbox_mountcheck::identity_drift(admitted, &|path| {
                crate::sandbox_mountcheck::mount_identity(std::path::Path::new(path))
            }),
        ) {
            crate::sandbox_mountcheck::PostCreate::Ready => {
                emit(SandboxPhase::PhaseDone);
                Ok(true)
            }
            crate::sandbox_mountcheck::PostCreate::NotRunning => Ok(false),
            crate::sandbox_mountcheck::PostCreate::Drift { why, path } => {
                remove_container(spec);
                let err = format!(
                    "sandbox mount drift after create: {why}; the container was removed and the launch refused"
                );
                emit(SandboxPhase::PhaseFailed { err: err.clone() });
                Err(anyhow::Error::new(refusal(
                    spec,
                    RefusalClass::MountDrift,
                    path,
                    err,
                )))
            }
        }
    }

    // A local Dockerfile build (devcontainer `build`) produces a tag no registry
    // has — skip the pull-prefetch; it's built below, right before create.
    if spec.build.is_none() {
        crate::sandbox_prefetch::prefetch_image(spec)?;
    }

    let rt = spec.backend.binary();

    // Single inspect call: are we running, and do policy and mounts match?
    let (running, mounts_ok) = container_status(spec);
    if running {
        if mounts_ok {
            return Ok(()); // already running with the correct mounts
        }
        // Stale policy or mounts — force-remove and fall through to recreate.
        msg::warn(&format!(
            "sandbox: container '{}' policy, image or mounts changed (config changed); recreating",
            spec.name
        ));
        remove_container(spec);
    }
    // Build the image now (synchronous, correct ordering) when a Dockerfile
    // build was requested — the tag is `spec.image`, so the run below finds it.
    crate::sandbox_build::build_image(spec)?;
    if !spec.volume_owners.is_empty() {
        let mut runner = |argv: &[String]| {
            let full = spec.placement.control_argv(argv);
            let timeout = if spec.placement.is_local() {
                PROBE_TIMEOUT
            } else {
                REMOTE_PROBE_TIMEOUT
            };
            output_and_stderr_with_timeout(&full, timeout)
        };
        crate::sandbox_volume_owner::claim_volumes(spec, &util::xdg_state_home(), &mut runner)
            .map_err(anyhow::Error::new)?;
    }
    let admitted = crate::sandbox_mountcheck::admitted_identities(spec);
    use crate::progress::{SandboxPhase, emit};
    emit(SandboxPhase::ContainerCreate {
        backend: spec.backend.label(),
    });
    let remote_stdin_secret_env = remote_stdin_oci_secret_env(spec);
    let (argv, stdin) =
        oci_create_argv_and_stdin(spec, oci_create_opts(spec)?, &remote_stdin_secret_env);
    // Keep the create's stderr: it is the only place the runtime says WHY, and
    // for the commonest macOS failure (a bind the VM cannot resolve) it names the
    // exact path. `run_control_owned` would drop it.
    let envfile_dir = util::xdg_state_home().join("thegn/sandbox-env");
    let envfile = oci_env_file_arg(&argv);
    let create_result = match stdin {
        Some(input) => stderr_with_timeout_stdin(&argv, RUN_TIMEOUT, input),
        None => stderr_with_timeout(&argv, RUN_TIMEOUT),
    };
    cleanup_oci_env_file(&envfile_dir, envfile.as_deref());
    let create_err = create_result.map(|(_, e)| e).unwrap_or_default();
    // Don't trust the exit code of `podman run -d`: on NixOS with broken
    // --userns keep-id, crun exits 0 but leaves the container in "created"
    // state. Verify it is actually running before declaring success.
    if post_create_check(spec, &admitted)? {
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
        let (retry, stdin) = oci_create_argv_and_stdin(
            spec,
            oci_create_opts_with_keep_id(spec, false)?,
            &remote_stdin_secret_env,
        );
        let retry_envfile = oci_env_file_arg(&retry);
        match stdin {
            Some(input) => drop(stderr_with_timeout_stdin(&retry, RUN_TIMEOUT, input)),
            None => drop(stderr_with_timeout(&retry, RUN_TIMEOUT)),
        }
        cleanup_oci_env_file(&envfile_dir, retry_envfile.as_deref());
        if post_create_check(spec, &admitted)? {
            msg::warn(
                "podman --userns keep-id failed; continuing with rootless podman default user namespace",
            );
            return Ok(());
        }
    }

    let err = format!("could not start {rt} container '{}'", spec.name);
    emit(SandboxPhase::PhaseFailed { err: err.clone() });
    anyhow::bail!(err)
}

/// Tear down the container for a worktree identified only by its local path.
/// Tries all OCI backends; logs best-effort failures at debug level. Intended for background
/// cleanup when a worktree is closed and only its path is known (no cfg/loc).
pub fn teardown_by_path(worktree: &str) {
    let name = container_name(worktree);
    let state_home = util::xdg_state_home();
    let projects =
        crate::sandbox_compose::teardown_projects_in(&state_home, std::path::Path::new(worktree));
    let placement = Placement::Local;
    for b in Backend::oci_runtimes() {
        if available(&placement, b) == RuntimeProbe::Present {
            run_teardown_argvs(&placement, b, teardown_argvs(b, None, &name, &projects));
        }
    }
    let record = state_home.join("thegn/compose-projects").join(format!(
        "{}.adopted",
        crate::sandbox_compose::hashed_project_name(&name, std::path::Path::new(worktree))
    ));
    if let Err(error) = std::fs::remove_file(record)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        tracing::debug!(target: "thegn::sandbox", %error, "unable to remove Compose adoption record");
    }
}

/// Remove a worktree's persistent container (OCI backends). Best-effort. Runs on
/// the worktree's host (local or remote, per its `GitLoc`).
pub fn teardown(cfg: &SandboxConfig, loc: &GitLoc, name: &str) {
    let placement = placement_from_loc(cfg, loc);
    teardown_placed(cfg, &placement, loc, name);
}

/// Remove a worktree's persistent container using its already-resolved placement.
/// The worktree destroy path uses this when a named environment selected SSH.
pub fn teardown_placed(cfg: &SandboxConfig, placement: &Placement, loc: &GitLoc, name: &str) {
    if let Err(error) = teardown_placed_checked(cfg, placement, loc, name) {
        tracing::warn!(target: "thegn::sandbox", %error, container = name, "sandbox teardown incomplete");
    }
}

/// Why a placed teardown could not reach a remote OCI runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlacedTeardownError {
    NoRuntime,
    Unreachable { commands: usize },
}

impl std::fmt::Display for PlacedTeardownError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoRuntime => f.write_str("no OCI runtime answered on the worktree's host"),
            Self::Unreachable { commands } => write!(
                f,
                "{commands} teardown command(s) could not reach the worktree's host or timed out"
            ),
        }
    }
}

impl std::error::Error for PlacedTeardownError {}

/// Remove a placed container, reporting remote runtime absence or unreachable commands.
pub fn teardown_placed_checked(
    cfg: &SandboxConfig,
    placement: &Placement,
    loc: &GitLoc,
    name: &str,
) -> Result<(), PlacedTeardownError> {
    let state_home = util::xdg_state_home();
    teardown_placed_with(
        cfg,
        placement,
        loc,
        name,
        &state_home,
        |backend| available(placement, backend),
        run_control_t_owned,
    )
}

fn teardown_placed_with(
    cfg: &SandboxConfig,
    placement: &Placement,
    loc: &GitLoc,
    name: &str,
    state_home: &std::path::Path,
    mut probe: impl FnMut(Backend) -> RuntimeProbe,
    mut run: impl FnMut(&Placement, &[String], Duration) -> Option<bool>,
) -> Result<(), PlacedTeardownError> {
    if !cfg.enabled {
        return Ok(());
    }
    // Drive the same remote daemon the create used, or `None` for the local
    // daemon; without this an `[sandbox] oci_host` container is never removed
    // (rm hits the local daemon, which never held it) and leaks forever.
    let oci_host = (!cfg.oci_host.trim().is_empty()).then(|| cfg.oci_host.trim());
    let projects = teardown_projects_for_loc(state_home, loc, name, !placement.is_local());
    // Try whichever OCI runtimes are available; the container only exists under one.
    let mut present = false;
    let mut unreachable = 0;
    for b in Backend::oci_runtimes() {
        if probe(b) == RuntimeProbe::Present {
            present = true;
            for argv in teardown_argvs(b, oci_host, name, &projects) {
                let timeout = if argv.iter().any(|arg| arg == "down") {
                    crate::sandbox_compose::COMPOSE_DOWN_TIMEOUT
                } else {
                    PROBE_TIMEOUT
                };
                match run(placement, &argv, timeout) {
                    Some(true) => {}
                    Some(false) => {
                        tracing::debug!(backend = b.label(), "sandbox teardown command failed")
                    }
                    None => {
                        unreachable += 1;
                        tracing::debug!(
                            backend = b.label(),
                            "sandbox teardown command unavailable or timed out"
                        );
                    }
                }
            }
        }
    }
    if !placement.is_local() && !present {
        return Err(PlacedTeardownError::NoRuntime);
    }
    if unreachable > 0 {
        return Err(PlacedTeardownError::Unreachable {
            commands: unreachable,
        });
    }
    Ok(())
}

/// Resolve teardown project paths from a Git location and its remote mount mode.
pub(crate) fn teardown_projects_for_loc(
    state_home: &std::path::Path,
    loc: &GitLoc,
    name: &str,
    remote: bool,
) -> Vec<String> {
    let loc_path = loc.path();
    let mut worktrees = vec![std::path::Path::new(&loc_path)];
    if remote {
        // remote_sync::retarget_if_remote_oci calls host::mount_remote_worktree
        // with remote_sync::REMOTE_WORKTREE_DEST for materialized checkouts.
        worktrees.push(std::path::Path::new("/workspace"));
    }
    crate::sandbox_compose::teardown_projects_for(state_home, name, &worktrees)
}

fn run_teardown_argvs(placement: &Placement, backend: Backend, argvs: Vec<Vec<String>>) {
    for argv in argvs {
        let timeout = if argv.iter().any(|arg| arg == "down") {
            crate::sandbox_compose::COMPOSE_DOWN_TIMEOUT
        } else {
            PROBE_TIMEOUT
        };
        match run_control_t_owned(placement, &argv, timeout) {
            Some(true) => {}
            Some(false) => {
                tracing::debug!(backend = backend.label(), "sandbox teardown command failed")
            }
            None => tracing::debug!(
                backend = backend.label(),
                "sandbox teardown command unavailable or timed out"
            ),
        }
    }
}

/// Build container and Compose cleanup commands for one runtime. Compose down
/// is intentionally separate from `rm -f`: it removes the project network and
/// any orphan services while using the same OCI daemon connection.
pub(crate) fn teardown_argvs(
    backend: Backend,
    oci_host: Option<&str>,
    name: &str,
    compose_projects: &[String],
) -> Vec<Vec<String>> {
    let agent = agent_container_name(name);
    let vpn = vpn_sidecar_name(name);
    let mut remove = oci_prefix_for(backend, oci_host);
    remove.extend(["rm".into(), "-f".into(), name.into(), agent, vpn]);
    let mut commands = vec![remove];
    for project in compose_projects {
        if let Some(down) = crate::sandbox_compose::down_argv_for(backend, oci_host, project) {
            commands.push(down);
        }
    }
    commands
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
    crate::thread_name::spawn_named("thegn-sbx-hooks", move || {
        for c in &cmds {
            // `detached`: null stdio + own group so a hook can't steal the tty.
            // best-effort: prepare hook: advisory; a failing hook doesn't block the launch
            let _ = crate::util::detached("sh")
                .arg("-lc")
                .arg(c)
                .current_dir(&wt)
                .status();
        }
    });
}

/// Build the full argv for an interactive pane running `inner` (a shell command
/// string, e.g. `${SHELL:-/bin/sh} -l` or `claude`). Callers must propagate a
/// volume refusal; it must never become host fallback or a shell shim. The
/// backend invocation is wrapped in mosh/ssh when the placement is remote.
pub fn enter_argv(spec: &SandboxSpec, inner: &str) -> Result<Vec<String>, EnterError> {
    enter_argv_guarded(spec, inner).map(EnterArgv::into_spawned)
}

/// Build an interactive pane argv while retaining ownership of any systemd
/// secret env-file until the caller hands the argv to a spawned unit. Callers
/// that may fail or decline to spawn after building should use this function.
pub fn enter_argv_guarded(spec: &SandboxSpec, inner: &str) -> Result<EnterArgv, EnterError> {
    let spec = &*launchable(spec)?;
    let script = wrap_script(spec, inner);
    // A compose-backed spec with a named service attaches through
    // `docker compose exec <service>` — no container-name guessing.
    let mut entered = if let Some(compose) = spec
        .compose_spec()
        .filter(|c| spec.backend.is_oci() && c.has_service())
    {
        let target = crate::sandbox_compose::ComposeTarget::for_spec(spec)
            .map_err(|error| refusal(spec, RefusalClass::ComposeTarget, None, error.to_string()))?;
        let workdir = (spec.file_access != FileAccess::None)
            .then(|| spec.worktree.to_string_lossy().into_owned());
        let argv = crate::sandbox_compose::exec_argv(
            &target,
            &crate::sandbox_compose::project_for(spec),
            &compose,
            workdir.as_deref(),
            &script,
            true,
        )
        .ok_or_else(|| {
            refusal(
                spec,
                RefusalClass::ComposeServiceMissing,
                None,
                "Compose service is missing".into(),
            )
        })?;
        EnterArgv {
            argv,
            env_file: None,
        }
    } else {
        backend_enter_argv_guarded(spec, &script)?
    };
    // Cap the pane's CPU on host-toolchain backends (no-op unless configured; see
    // [`crate::sandbox_cpucap`]). OCI/Systemd cap inline in their backend argv.
    entered.argv = crate::sandbox_cpucap::wrap_pane_argv(spec, entered.argv);
    entered.argv = spec.placement.interactive_argv(&entered.argv);
    Ok(entered)
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

/// The extra read-only bind a bwrap sandbox needs so `/etc/resolv.conf`
/// resolves (THE-730), or `None`. `None` for `network = none`, when the target
/// is already reachable, or when a configured mount already covers the
/// directory (e.g. a `/run/systemd/resolve:ro` workaround): no duplicate bind.
fn bwrap_resolver_bind(spec: &SandboxSpec, hardcoded_parents: &[&str]) -> Option<String> {
    let dir = crate::sandbox_mounts::resolver_bind_dir(
        std::path::Path::new("/etc/resolv.conf"),
        std::path::Path::new("/run"),
        BWRAP_SUBSTRATE,
    )?;
    resolver_bind_filter(spec, hardcoded_parents, &dir)
}

/// Pure policy half of [`bwrap_resolver_bind`], with the resolved dir injected.
fn resolver_bind_filter(
    spec: &SandboxSpec,
    hardcoded_parents: &[&str],
    dir: &std::path::Path,
) -> Option<String> {
    if spec.network == Network::None {
        return None;
    }
    let covered = |p: &str| dir.starts_with(p);
    if hardcoded_parents.iter().any(|p| covered(p)) || spec.mounts.iter().any(|m| covered(&m.dest))
    {
        return None;
    }
    Some(dir.to_string_lossy().into_owned())
}

/// Build the complete list of systemd path properties emitted for this spec.
/// Keeping selection and encoding together ensures launch admission checks
/// exactly the paths that systemd-run will receive.
fn systemd_mount_props(spec: &SandboxSpec) -> Result<Vec<String>, String> {
    let sealed = spec
        .drop_capabilities
        .iter()
        .any(|c| c.eq_ignore_ascii_case("ALL"));
    let carveouts = if spec.read_only_root && spec.seal_home.is_none() {
        default_writable_carveouts(if sealed {
            SandboxProfile::Sealed
        } else {
            SandboxProfile::Hardened
        })
    } else {
        Vec::new()
    };
    systemd_mount_props_with_carveouts(spec, &carveouts)
}

fn systemd_unsafe_mount_path(spec: &SandboxSpec) -> Option<String> {
    spec.mounts.iter().find_map(|mount| {
        [mount.host.as_str(), mount.dest.as_str()]
            .into_iter()
            .find(|path| crate::sandbox_mounts::systemd_path_refusal(path).is_some())
            .map(str::to_owned)
    })
}

fn sealed_home_offending_path(spec: &SandboxSpec) -> Option<String> {
    let home = std::path::Path::new(spec.seal_home.as_deref()?);
    spec.mounts
        .iter()
        .find(|mount| {
            crate::sandbox_mounts::sealed_mount_covers_deny(mount, home)
                || (spec.backend.profile().family == BackendFamily::Systemd
                    && std::path::Path::new(&mount.dest).starts_with(home)
                    && (crate::sandbox_mounts::systemd_path_refusal(&mount.host).is_some()
                        || crate::sandbox_mounts::systemd_path_refusal(&mount.dest).is_some()))
        })
        .map(|mount| mount.dest.clone())
}

fn systemd_mount_props_with_carveouts(
    spec: &SandboxSpec,
    carveouts: &[Mount],
) -> Result<Vec<String>, String> {
    let mut props = Vec::new();
    if spec.read_only_root {
        let under_home = |dest: &str| {
            spec.seal_home
                .as_deref()
                .is_some_and(|h| std::path::Path::new(dest).starts_with(h))
        };
        for m in &spec.mounts {
            if under_home(&m.dest) {
                let flag = if m.ro {
                    "BindReadOnlyPaths"
                } else {
                    "BindPaths"
                };
                props.push(crate::sandbox_mounts::systemd_bind_property(
                    flag, &m.host, &m.dest,
                )?);
            } else if !m.ro {
                props.push(crate::sandbox_mounts::systemd_path_property(
                    "ReadWritePaths",
                    &m.dest,
                )?);
            }
        }
        // $HOME is otherwise read-only; carve the same narrow writable state
        // dirs as bwrap. Sealed profiles omit these because HOME is a tmpfs.
        for cv in carveouts {
            props.push(crate::sandbox_mounts::systemd_path_property(
                "ReadWritePaths",
                &cv.dest,
            )?);
        }
    }
    // systemd does not consume read-only mounts generally; preserve the
    // special shared .git/config protection used by this backend.
    for m in &spec.mounts {
        if m.ro && m.dest.ends_with("/config") {
            props.push(crate::sandbox_mounts::systemd_path_property(
                "ReadOnlyPaths",
                &m.dest,
            )?);
        }
    }
    Ok(props)
}

fn bwrap_mount_covered(mount: &Mount, hardcoded_parents: &[&str]) -> bool {
    hardcoded_parents
        .iter()
        .any(|p| std::path::Path::new(&mount.dest).starts_with(p) && mount.dest != *p)
}

/// The backend-specific argv that runs `/bin/sh -lc <script>` in the sandbox.
#[cfg(test)]
fn backend_enter_argv(spec: &SandboxSpec, script: &str) -> Result<Vec<String>, EnterError> {
    backend_enter_argv_with_env(
        spec,
        script,
        SandboxArgvEnv {
            resolver_dir: None,
            runtime_dir: systemd_runtime_dir(std::env::var_os("XDG_RUNTIME_DIR")),
            control_dirs: Vec::new(),
        },
    )
}

fn backend_enter_argv_guarded(spec: &SandboxSpec, script: &str) -> Result<EnterArgv, EnterError> {
    backend_enter_argv_guarded_with_env(
        spec,
        script,
        SandboxArgvEnv {
            resolver_dir: None,
            runtime_dir: systemd_runtime_dir(std::env::var_os("XDG_RUNTIME_DIR")),
            control_dirs: crate::sandbox_control_guard::control_dirs(),
        },
    )
}

fn systemd_runtime_dir(value: Option<std::ffi::OsString>) -> Option<std::path::PathBuf> {
    value
        .map(std::path::PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
        .or_else(|| Some(std::path::PathBuf::from("/run/user")))
}

/// Build backend argv with an optional injected resolver directory for tests.
/// Production callers leave it unset and use the host resolver probe.
#[cfg(test)]
fn backend_enter_argv_with_resolver_dir(
    spec: &SandboxSpec,
    script: &str,
    resolved_resolver_dir: Option<&std::path::Path>,
) -> Result<Vec<String>, EnterError> {
    backend_enter_argv_with_env(
        spec,
        script,
        SandboxArgvEnv {
            resolver_dir: resolved_resolver_dir.map(std::path::Path::to_path_buf),
            runtime_dir: None,
            control_dirs: Vec::new(),
        },
    )
}

#[derive(Default)]
struct SandboxArgvEnv {
    resolver_dir: Option<std::path::PathBuf>,
    runtime_dir: Option<std::path::PathBuf>,
    control_dirs: Vec<std::path::PathBuf>,
}

#[cfg(test)]
fn backend_enter_argv_with_env(
    spec: &SandboxSpec,
    script: &str,
    env: SandboxArgvEnv,
) -> Result<Vec<String>, EnterError> {
    backend_enter_argv_guarded_with_env(spec, script, env).map(EnterArgv::into_spawned)
}

fn backend_enter_argv_guarded_with_env(
    spec: &SandboxSpec,
    script: &str,
    env: SandboxArgvEnv,
) -> Result<EnterArgv, EnterError> {
    let wt = spec.worktree.to_string_lossy().into_owned();
    let mut env_file: Option<SecretEnvFile> = None;
    // Keyed on the backend *family* (one arm per way of driving a sandbox),
    // so a new OCI runtime is a profile row, not a new arm here.
    let argv = match spec.backend.profile().family {
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
            // Sealed: a private tmpfs `$HOME` BEFORE any mount, so the allowlist,
            // worktree and caches (all path-preserving, mostly under `$HOME`)
            // land inside it and nothing else of the host `$HOME` exists.
            if let Some(home) = &spec.seal_home
                && !matches!(spec.file_access, FileAccess::All | FileAccess::Host)
            {
                v.extend(["--tmpfs".into(), home.clone()]);
            }
            // THE-730: `/etc` is bound but `/run` is not, so a resolv.conf that
            // symlinks into `/run/systemd/resolve` dangles and the sandbox has no
            // DNS. Bind the resolved target's directory read-only. Never for
            // `network = none` (no resolver reachable by design), nor for a
            // remote placement (the probe would read the wrong host).
            let resolver_dir = if spec.placement.is_local()
                && !matches!(spec.file_access, FileAccess::All | FileAccess::Host)
            {
                match env.resolver_dir.as_deref() {
                    Some(dir) => resolver_bind_filter(spec, &hardcoded_parents, dir),
                    None => bwrap_resolver_bind(spec, &hardcoded_parents),
                }
            } else {
                None
            };
            if let Some(dir) = &resolver_dir {
                v.extend(["--ro-bind".into(), dir.clone(), dir.clone()]);
                // Nested user mounts cannot be created beneath this read-only
                // bind, so let the normal covered-mount skip handle them.
                hardcoded_parents.push(dir);
            }
            let mut emitted_mount_dests = Vec::new();
            for m in &spec.mounts {
                // Skip mounts already covered by a hardcoded parent — bwrap
                // cannot create a mount-point inside a read-only bind.
                let covered = bwrap_mount_covered(m, &hardcoded_parents);
                if covered {
                    if resolver_dir
                        .as_deref()
                        .is_some_and(|dir| std::path::Path::new(&m.dest).starts_with(dir))
                    {
                        warn_dropped_mount_once(
                            &m.host,
                            &m.dest,
                            "bwrap resolver bind covers the mount destination",
                        );
                    }
                    continue;
                }
                // Also skip exact duplicates of already-hardcoded paths.
                let duplicate = hardcoded_parents.iter().any(|p| m.dest == *p);
                if duplicate {
                    continue;
                }
                let flag = if m.ro { "--ro-bind" } else { "--bind" };
                v.extend([flag.into(), m.host.clone(), m.dest.clone()]);
                emitted_mount_dests.push(std::path::Path::new(&m.dest));
            }
            if spec.placement.is_local()
                && !matches!(spec.file_access, FileAccess::All | FileAccess::Host)
            {
                let mut visible_roots: Vec<&std::path::Path> = hardcoded_parents
                    .iter()
                    .map(|path| std::path::Path::new(path))
                    .collect();
                visible_roots.extend(emitted_mount_dests.iter().copied());
                let mut bind_dests = emitted_mount_dests;
                bind_dests.push(&spec.worktree);
                match crate::sandbox_control_guard::plan(
                    &env.control_dirs,
                    &visible_roots,
                    &bind_dests,
                    &|path| std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_dir()),
                ) {
                    crate::sandbox_control_guard::MaskPlan::Clear => {}
                    crate::sandbox_control_guard::MaskPlan::Mask(dirs) => {
                        for dir in dirs {
                            v.extend(["--tmpfs".into(), dir.display().to_string()]);
                        }
                    }
                    crate::sandbox_control_guard::MaskPlan::Refuse(exposure) => {
                        return Err(refusal(
                            spec,
                            RefusalClass::ControlSocketReachable,
                            Some(exposure.control_dir.display().to_string()),
                            format!(
                                "daemon control directory {} is visible through sandbox bind {}; bwrap could not mask it without hiding a bind",
                                exposure.control_dir.display(),
                                exposure.visible_through.display()
                            ),
                        ));
                    }
                }
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
                let sealed_home = spec.seal_home.is_some();
                // Sealed: `ProtectHome=tmpfs` leaves an empty `$HOME`; the
                // allowlist, worktree and caches are bound back in below.
                let protect_home = if sealed_home { "tmpfs" } else { "read-only" };
                v.extend(["-p".into(), format!("ProtectHome={protect_home}")]);
                let mut runtime_control_dir = None;
                if let Some(runtime_dir) = env.runtime_dir.as_deref() {
                    let control_dir = if runtime_dir == std::path::Path::new("/run/user") {
                        runtime_dir.to_path_buf()
                    } else {
                        runtime_dir.join("thegn")
                    };
                    runtime_control_dir = Some(control_dir.clone());
                    v.extend([
                        "-p".into(),
                        format!("InaccessiblePaths=-{}", control_dir.display()),
                    ]);
                }
                for control_dir in &env.control_dirs {
                    if runtime_control_dir
                        .as_ref()
                        .is_some_and(|runtime| control_dir.starts_with(runtime))
                    {
                        continue;
                    }
                    v.extend([
                        "-p".into(),
                        format!("InaccessiblePaths=-{}", control_dir.display()),
                    ]);
                }
            }
            for property in systemd_mount_props(spec).map_err(|message| {
                refusal(
                    spec,
                    RefusalClass::SystemdMountPath,
                    systemd_unsafe_mount_path(spec),
                    message,
                )
            })? {
                v.extend(["-p".into(), property]);
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
            // Host-sourced secrets go to a 0600 `EnvironmentFile=` property so
            // they stay off systemd-run's argv, which persists for the pane's
            // whole lifetime in `/proc/*/cmdline`; synthetic/non-secret pairs
            // stay inline as `--setenv K=V`. (Remote specs keep everything
            // inline — see `partition_secret_env`.)
            let (inline_env, secret_env) = partition_secret_env(spec);
            let state_home = util::xdg_state_home();
            if let Some(envfile) = write_secret_env_file_in(
                &state_home,
                std::path::Path::new("thegn/sandbox-env"),
                &spec.name,
                &secret_env,
                true,
            ) {
                let dir = state_home.join("thegn/sandbox-env");
                let guard = SecretEnvFile::adopt(dir, envfile);
                v.extend([
                    "-p".into(),
                    format!("EnvironmentFile={}", guard.path().display()),
                ]);
                match util::which_path("rm")
                    .and_then(|rm| systemd_env_cleanup_prop(&rm, guard.path()))
                {
                    Some(property) => v.extend(["-p".into(), property]),
                    None => {
                        return Err(refusal(
                            spec,
                            RefusalClass::SystemdMountPath,
                            Some(guard.path().display().to_string()),
                            "the systemd secret env file path cannot be expressed in an ExecStopPost cleanup (it contains $, % , ; or a control character, or rm was not found as a plain absolute path); refusing to launch rather than leave the secret file behind".into(),
                        ));
                    }
                }
                env_file = Some(guard);
            } else if !secret_env.is_empty() {
                // Never put host-sourced secrets back on argv if the protected
                // file cannot be created. A failed tool is safer than a
                // credential leak.
                tracing::error!(
                    target: "thegn::sandbox",
                    name = %spec.name,
                    "unable to create protected systemd environment file; omitting host-sourced secrets"
                );
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
    };
    Ok(EnterArgv { argv, env_file })
}

/// Borrowed `(key, value)` env pairs from a spec's `env` map.
type EnvPairs<'a> = Vec<(&'a String, &'a String)>;

/// Split `spec.env` into inline and secret pairs for local placement.
/// A pair whose value matches the launcher's own process env is a
/// host-sourced passthrough secret (GH_TOKEN, API keys — see `resolve`'s
/// env_passthrough) and MUST NOT ride the world-readable argv (`-e K=V` /
/// `--setenv K=V` are visible in `/proc/*/cmdline`); it goes to a 0600
/// env-file instead. Synthetic pairs (THEGN_SANDBOX, NIX_REMOTE — values absent
/// from the host env) are not secrets and stay inline. SSH OCI secrets are
/// classified separately for stdin delivery; other remote placements retain
/// inline delivery.
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

fn remote_stdin_oci_secret_env(spec: &SandboxSpec) -> EnvPairs<'_> {
    if !spec.placement.control_forwards_stdin() {
        return Vec::new();
    }
    spec.env
        .iter()
        .filter(|(key, value)| std::env::var(key).ok().as_deref() == Some(value.as_str()))
        .map(|(key, value)| (key, value))
        .collect()
}

/// Add the final run command and wrap it for its placement. The returned stdin
/// is present only for stdin-capable remote OCI host-sourced secrets, and is consumed by the
/// same control call that carries the command.
fn oci_create_argv_and_stdin(
    spec: &SandboxSpec,
    opts: Vec<String>,
    secret_env: &[(&String, &String)],
) -> (Vec<String>, Option<Vec<u8>>) {
    let mut argv = oci_prefix(spec);
    argv.extend([
        "run".into(),
        "-d".into(),
        "--name".into(),
        spec.name.clone(),
    ]);
    argv.extend(opts);
    argv.extend(entrypoint_override());
    argv.push(effective_image(spec));
    argv.push("infinity".into());
    let input = secret_env
        .iter()
        .map(|(key, value)| format!("{key}={value}\n"))
        .collect::<String>()
        .into_bytes();
    let input = (!input.is_empty()).then_some(input);
    (spec.placement.control_argv(&argv), input)
}

/// Write host-sourced secret env pairs to a 0600 per-sandbox env-file and return
/// its path. On failure, callers omit secrets instead of leaking them to argv.
fn write_secret_env_file(
    name: &str,
    secret: &[(&String, &String)],
    generation: bool,
) -> Option<PathBuf> {
    if secret.is_empty() {
        return None;
    }
    let anchor = util::xdg_state_home();
    write_secret_env_file_in(
        &anchor,
        std::path::Path::new("thegn/sandbox-env"),
        name,
        secret,
        generation,
    )
}

fn write_secret_env_file_in(
    anchor: &std::path::Path,
    rel: &std::path::Path,
    name: &str,
    secret: &[(&String, &String)],
    generation: bool,
) -> Option<PathBuf> {
    if secret.is_empty() {
        return None;
    }
    std::fs::create_dir_all(anchor).ok()?;
    tighten_owned_state_dir(&anchor.join("thegn"));
    let dir = crate::fsperm::ensure_private_dir_beneath(anchor, rel).ok()?;
    let file_name = if generation {
        static NEXT_ENV_FILE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let id = NEXT_ENV_FILE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        format!("{name}-{}-{id}.env", std::process::id())
    } else {
        format!("{name}.env")
    };
    let body: String = secret.iter().map(|(k, v)| format!("{k}={v}\n")).collect();
    match dir.write_owner_only_atomic(&file_name, body.as_bytes()) {
        Ok(path) => Some(path),
        Err(error) => {
            tracing::debug!(target: "thegn::sandbox", %error, file = %file_name, "secret env-file write failed");
            None
        }
    }
}

/// State directories created under a user-private-group umask can be 0775.
/// Tighten thegn's own directory before the secure walk, while leaving the
/// trusted state anchor and any foreign or symlinked directory untouched.
fn tighten_owned_state_dir(_path: &std::path::Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        if let Ok(metadata) = std::fs::symlink_metadata(_path)
            && metadata.file_type().is_dir()
            && metadata.uid() == nix::unistd::geteuid().as_raw()
            && metadata.mode() & 0o022 != 0
        {
            use nix::fcntl::{OFlag, open};
            use nix::sys::stat::{Mode, SFlag, fchmod, fstat};

            let tightened = (|| {
                let directory = open(
                    _path,
                    OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
                    Mode::empty(),
                )
                .map_err(std::io::Error::other)?;
                let metadata = fstat(&directory).map_err(std::io::Error::other)?;
                if !SFlag::from_bits_truncate(metadata.st_mode).contains(SFlag::S_IFDIR)
                    || metadata.st_uid != nix::unistd::geteuid().as_raw()
                    || metadata.st_mode & 0o022 == 0
                {
                    return Ok(());
                }
                fchmod(
                    &directory,
                    Mode::from_bits_truncate(metadata.st_mode & !0o022),
                )
                .map_err(std::io::Error::other)
            })();
            if let Err(error) = tightened {
                tracing::debug!(path = %_path.display(), %error, "could not tighten sandbox state directory permissions");
            }
        }
    }
}

fn systemd_env_cleanup_prop(rm: &str, path: &std::path::Path) -> Option<String> {
    if !std::path::Path::new(rm).is_absolute() || !path.is_absolute() {
        return None;
    }
    let path = path.to_str()?;
    let rm_is_plain = rm
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '/' | '.' | '_' | '+' | '-'));
    if !rm_is_plain
        || path
            .chars()
            .any(|ch| ch.is_control() || matches!(ch, '$' | '%' | ';'))
    {
        return None;
    }
    let path_is_plain = path
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '/' | '.' | '_' | '+' | '-'));
    if path_is_plain {
        Some(format!("ExecStopPost=-+{rm} -f -- {path}"))
    } else {
        let escaped = path.replace('\\', "\\\\").replace('"', "\\\"");
        Some(format!("ExecStopPost=-+{rm} -f -- \"{escaped}\""))
    }
}

/// Remove secret env-file residue older than this bound. A crash can therefore
/// leave plaintext credentials for at most one hour before the next startup GC.
const SECRET_ENV_FILE_MAX_AGE: Duration = Duration::from_secs(60 * 60);

fn sweep_stale_env_files_in(
    anchor: &std::path::Path,
    rel: &std::path::Path,
    now: SystemTime,
    max_age: Duration,
) -> usize {
    let dir = anchor.join(rel);
    match std::fs::symlink_metadata(&dir) {
        Ok(metadata) if metadata.file_type().is_dir() => {}
        Ok(_) => return 0,
        Err(error) => {
            tracing::debug!(target: "thegn::sandbox", %error, "unable to inspect sandbox env-file directory");
            return 0;
        }
    }
    if rel.starts_with("thegn") {
        tighten_owned_state_dir(&anchor.join("thegn"));
    }
    match crate::fsperm::ensure_private_dir_beneath(anchor, rel) {
        Ok(dir) => dir.remove_stale_files("env", now, max_age),
        Err(error) => {
            tracing::debug!(target: "thegn::sandbox", %error, "unable to pin sandbox env-file directory");
            0
        }
    }
}

fn sweep_orphan_vpn_staging_in(
    anchor: &std::path::Path,
    rel: &std::path::Path,
    listed: Option<&std::collections::HashSet<String>>,
    removed: &[String],
    now: SystemTime,
    max_age: Duration,
) -> usize {
    let root = anchor.join(rel);
    match std::fs::symlink_metadata(&root) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
        Ok(_) => return 0,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return 0,
        Err(error) => {
            tracing::debug!(target: "thegn::sandbox", %error, "unable to inspect VPN staging directory");
            return 0;
        }
    }
    let _pinned = match crate::fsperm::ensure_private_dir_beneath(anchor, rel) {
        Ok(dir) => dir,
        Err(error) => {
            tracing::debug!(target: "thegn::sandbox", %error, "unable to pin VPN staging directory");
            return 0;
        }
    };
    let entries = match std::fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(error) => {
            tracing::debug!(target: "thegn::sandbox", %error, "unable to list VPN staging directory");
            return 0;
        }
    };
    let mut count = 0;
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                tracing::debug!(target: "thegn::sandbox", %error, "unable to inspect VPN staging entry");
                continue;
            }
        };
        let name = entry.file_name();
        let Some(name_text) = name.to_str() else {
            continue;
        };
        if !name_text.ends_with(VPN_SIDECAR_SUFFIX) {
            continue;
        }
        let path = root.join(&name);
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) => {
                tracing::debug!(target: "thegn::sandbox", %error, "unable to inspect VPN staging entry");
                continue;
            }
        };
        if metadata.file_type().is_symlink() {
            if removed.iter().any(|removed_name| removed_name == name_text) {
                match std::fs::remove_file(&path) {
                    Ok(()) => count += 1,
                    Err(error) => {
                        tracing::debug!(target: "thegn::sandbox", %error, "unable to unlink VPN staging symlink")
                    }
                }
            }
            continue;
        }
        if !metadata.is_dir() {
            continue;
        }
        let deleted_by_gc = removed.iter().any(|removed_name| removed_name == name_text);
        let old_unlisted = listed.is_some_and(|listed| {
            !listed.contains(name_text)
                && metadata
                    .modified()
                    .ok()
                    .and_then(|modified| now.duration_since(modified).ok())
                    .is_some_and(|age| age >= max_age)
        });
        if !deleted_by_gc && !old_unlisted {
            continue;
        }
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
            Ok(_) => continue,
            Err(error) => {
                tracing::debug!(target: "thegn::sandbox", %error, "unable to recheck VPN staging directory");
                continue;
            }
        };
        let files = match std::fs::read_dir(&path) {
            Ok(files) => files,
            Err(error) => {
                tracing::debug!(target: "thegn::sandbox", %error, "unable to list VPN staging files");
                continue;
            }
        };
        let mut has_residue = false;
        for file in files {
            let file = match file {
                Ok(file) => file,
                Err(error) => {
                    tracing::debug!(target: "thegn::sandbox", %error, "unable to inspect VPN staging file");
                    has_residue = true;
                    continue;
                }
            };
            let file_path = path.join(file.file_name());
            match std::fs::symlink_metadata(&file_path) {
                Ok(metadata) if metadata.is_file() || metadata.file_type().is_symlink() => {
                    if let Err(error) = std::fs::remove_file(&file_path) {
                        tracing::debug!(target: "thegn::sandbox", %error, "unable to remove VPN staging file");
                        has_residue = true;
                    }
                }
                Ok(_) => {
                    tracing::debug!(target: "thegn::sandbox", path = %file_path.display(), "keeping non-file VPN staging entry");
                    has_residue = true;
                }
                Err(error) => {
                    tracing::debug!(target: "thegn::sandbox", %error, "unable to recheck VPN staging file");
                    has_residue = true;
                }
            }
        }
        if has_residue {
            continue;
        }
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
            Ok(_) => continue,
            Err(error) => {
                tracing::debug!(target: "thegn::sandbox", %error, "unable to recheck VPN staging directory before removal");
                continue;
            }
        }
        match std::fs::remove_dir(&path) {
            Ok(()) => count += 1,
            Err(error) => {
                tracing::debug!(target: "thegn::sandbox", %error, "unable to remove VPN staging directory")
            }
        }
    }
    count
}

fn oci_env_file_arg(argv: &[String]) -> Option<PathBuf> {
    argv.windows(2)
        .find(|pair| pair[0] == "--env-file")
        .map(|pair| PathBuf::from(&pair[1]))
}

fn remove_oci_env_file_in(dir: &std::path::Path, path: &std::path::Path) -> std::io::Result<bool> {
    if path.parent() != Some(dir)
        || path.extension().and_then(std::ffi::OsStr::to_str) != Some("env")
    {
        return Ok(false);
    }
    std::fs::remove_file(path)?;
    Ok(true)
}

fn cleanup_oci_env_file(dir: &std::path::Path, path: Option<&std::path::Path>) {
    if let Some(path) = path
        && let Err(error) = remove_oci_env_file_in(dir, path)
    {
        tracing::debug!(target: "thegn::sandbox", %error, "unable to remove OCI secret env-file");
    }
}

/// Owns a systemd secret env-file until its unit is spawned.
#[derive(Debug)]
pub struct SecretEnvFile {
    dir: PathBuf,
    path: PathBuf,
    armed: bool,
}

impl SecretEnvFile {
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// Transfer cleanup ownership to the spawned systemd unit's ExecStopPost.
    pub fn keep(mut self) -> PathBuf {
        self.armed = false;
        self.path.clone()
    }

    pub(crate) fn adopt(dir: PathBuf, path: PathBuf) -> Self {
        Self {
            dir,
            path,
            armed: true,
        }
    }
}

impl Drop for SecretEnvFile {
    fn drop(&mut self) {
        if self.armed
            && let Err(error) = remove_oci_env_file_in(&self.dir, &self.path)
        {
            tracing::debug!(
                target: "thegn::sandbox",
                %error,
                path = %self.path.display(),
                "unable to remove unspawned systemd secret env-file"
            );
        }
    }
}

/// Backend argv plus any secret env-file whose lifetime follows that argv.
#[derive(Debug)]
pub struct EnterArgv {
    argv: Vec<String>,
    env_file: Option<SecretEnvFile>,
}

impl EnterArgv {
    pub fn argv(&self) -> &[String] {
        &self.argv
    }

    /// Mark a launch as handed off; systemd's ExecStopPost removes its file.
    pub fn into_spawned(self) -> Vec<String> {
        if let Some(env_file) = self.env_file {
            env_file.keep();
        }
        self.argv
    }

    pub fn into_parts(self) -> (Vec<String>, Option<SecretEnvFile>) {
        (self.argv, self.env_file)
    }
}

/// OCI `run` options shared by the keep-alive container: mounts, network, env,
/// and uid mapping so bind-mounted files stay host-owned.
fn oci_create_opts(spec: &SandboxSpec) -> Result<Vec<String>, VolumeAdmissionError> {
    let remote_stdin_secret_env = remote_stdin_oci_secret_env(spec);
    oci_create_opts_with_remote_stdin_secrets(spec, &remote_stdin_secret_env)
}

fn oci_create_opts_with_remote_stdin_secrets(
    spec: &SandboxSpec,
    remote_stdin_secret_env: &[(&String, &String)],
) -> Result<Vec<String>, VolumeAdmissionError> {
    admit_volumes(spec)?;
    let spec = &*sealed_view(spec);
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
        v.extend(["--label".into(), crate::sandbox_manage::owner_label_arg()]);
        v.extend([
            "--label".into(),
            format!(
                "{}={}",
                crate::sandbox_manage::WORKTREE_LABEL_KEY,
                crate::sandbox_manage::worktree_label_value(
                    spec.worktree.as_os_str().as_encoded_bytes()
                )
            ),
        ]);
        v.extend([
            "--label".into(),
            format!(
                "{}={}",
                crate::sandbox_manage::ROLE_LABEL_KEY,
                crate::sandbox_manage::MAIN_ROLE
            ),
        ]);
        v.extend([
            "--label".into(),
            format!("{SEAL_LABEL}={}", u8::from(spec.seal_home.is_some())),
        ]);
        v.extend([
            "--label".into(),
            format!("{POLICY_LABEL}={}", expected_policy(spec)),
        ]);
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
    if emits_nix_bind(spec) {
        v.extend(["-v".into(), "/nix:/nix:ro".into()]);
    }
    // Host-sourced secrets stay off argv. Local creates use a protected file;
    // Stdin-capable remote placements feed the env-file body over the control
    // process stdin. Other remote placement kinds keep their existing inline delivery.
    let (inline_env, secret_env) = partition_secret_env(spec);
    let inline_env: EnvPairs<'_> = if remote_stdin_secret_env.is_empty() {
        inline_env
    } else {
        inline_env
            .into_iter()
            .filter(|pair| !remote_stdin_secret_env.contains(pair))
            .collect()
    };
    if spec.placement.control_forwards_stdin() && !remote_stdin_secret_env.is_empty() {
        v.extend(["--env-file".into(), "/dev/stdin".into()]);
    } else if let Some(envfile) = write_secret_env_file(&spec.name, &secret_env, true) {
        v.extend(["--env-file".into(), envfile.to_string_lossy().into_owned()]);
    } else if !secret_env.is_empty() {
        // Never put host-sourced secrets back on argv if the protected file
        // cannot be created. The sandbox runs without the secret in that rare
        // case; a failed tool is safer than a credential leak.
        tracing::error!(
            target: "thegn::sandbox",
            name = %spec.name,
            "unable to create protected OCI environment file; omitting host-sourced secrets"
        );
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
    Ok(v)
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
fn oci_create_opts_with_keep_id(
    spec: &SandboxSpec,
    keep_id: bool,
) -> Result<Vec<String>, VolumeAdmissionError> {
    let remote_stdin_secret_env = remote_stdin_oci_secret_env(spec);
    oci_create_opts_with_keep_id_and_remote_stdin_secrets(spec, keep_id, &remote_stdin_secret_env)
}

fn oci_create_opts_with_keep_id_and_remote_stdin_secrets(
    spec: &SandboxSpec,
    keep_id: bool,
    remote_stdin_secret_env: &[(&String, &String)],
) -> Result<Vec<String>, VolumeAdmissionError> {
    admit_volumes(spec)?;
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
    let mut full = oci_create_opts_with_remote_stdin_secrets(spec, remote_stdin_secret_env)?;
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
        Ok(out)
    } else {
        Ok(full)
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
    // A live worktree owns its plain `thegn-{slug}` container, the profile
    // variant `thegn-{profile}-{slug}`, the `-tgagent` / `-tgvpn` companions,
    // and Compose service replicas ending in `-{service}-{n}` or `_{service}_{n}`.
    // Reconcile by reverse-mapping each candidate back to a worktree slug rather
    // than allow-listing exact names: profile, sidecar, and service containers
    // must not be misread as orphans and force-removed while their worktree lives.
    // Reaping is fail-closed: any container that maps to an active worktree by any
    // of these forms is kept.
    let active_bases: Vec<Vec<String>> = active_worktrees
        .iter()
        .map(|worktree| {
            let slug = util::slugify(worktree);
            let hashed = format!("{slug}-{}", util::short_hash(worktree, 6));
            vec![slug, hashed]
        })
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
            // Keep plain/profile bases and service-replica suffixes for every
            // active slug; unknown forms remain eligible for orphan cleanup.
            !active_bases.iter().any(|bases| {
                bases.iter().any(|base| {
                    if rest == base || rest.ends_with(&format!("-{base}")) {
                        return true;
                    }
                    let suffix = rest.strip_prefix(base).or_else(|| {
                        rest.find(&format!("-{base}"))
                            .map(|index| &rest[index + base.len() + 1..])
                    });
                    let Some(suffix) = suffix else {
                        return false;
                    };
                    let Some(separator) =
                        suffix.chars().next().filter(|ch| *ch == '-' || *ch == '_')
                    else {
                        return false;
                    };
                    let service_and_replica = &suffix[separator.len_utf8()..];
                    let Some((service, replica)) = service_and_replica.rsplit_once(separator)
                    else {
                        return false;
                    };
                    // Known fail-closed over-keep: dead `wt-feat-fix-2` can look
                    // like service `fix` replica 2 while live `wt-feat` exists.
                    !service.is_empty()
                        && !replica.is_empty()
                        && replica.bytes().all(|byte| byte.is_ascii_digit())
                })
            })
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
/// Smol and Apple are list-only until their label-filter semantics are verified.
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
    let report = run_gc_detailed(db_worktrees);
    let unverified_count = report
        .unverified
        .iter()
        .map(|(_, names)| names.len())
        .sum::<usize>();
    if unverified_count > 0 {
        tracing::info!(
            target: "thegn::sandbox",
            count = unverified_count,
            "orphan GC kept unverified containers; run `thegn sandbox gc` to list them"
        );
    }
    report
        .removed
        .into_iter()
        .flat_map(|(_, names)| names)
        .collect()
}

/// [`run_gc`] results grouped by backend for the on-demand `thegn sandbox gc`
/// report. Only this profile's labelled orphans are removed; unverified
/// orphan-looking names are returned for the user to review. The startup sweep
/// uses the flattened removals and logs the unverified count.
pub struct GcReport {
    pub removed: Vec<(&'static str, Vec<String>)>,
    pub unverified: Vec<(&'static str, Vec<String>)>,
}

pub fn run_gc_detailed(db_worktrees: &[String]) -> GcReport {
    let state_home = util::xdg_state_home();
    sweep_stale_env_files_in(
        &state_home,
        std::path::Path::new("thegn/sandbox-env"),
        SystemTime::now(),
        SECRET_ENV_FILE_MAX_AGE,
    );
    crate::sandbox_compose::sweep_stale_label_files_in(
        &state_home,
        SystemTime::now(),
        SECRET_ENV_FILE_MAX_AGE,
    );
    let removed_adoption_records = crate::sandbox_compose::sweep_stale_adoption_records_in(
        &state_home,
        db_worktrees,
        SystemTime::now(),
        SECRET_ENV_FILE_MAX_AGE,
    );
    if removed_adoption_records > 0 {
        tracing::debug!(
            target: "thegn::sandbox",
            removed = removed_adoption_records,
            "swept stale Compose adoption records"
        );
    }
    let mut removed_report = Vec::new();
    let mut unverified_report = Vec::new();
    let mut listed_names = std::collections::HashSet::new();
    let mut removed_names = Vec::new();
    let mut listed_any = false;
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
        listed_any = true;
        listed_names.extend(containers.iter().cloned());
        let identity = crate::sandbox_manage::owner_identity();
        let mine = crate::sandbox_manage::gc_owned_list_argv(backend, &identity).and_then(|argv| {
            let mut command = backend_prefix(backend);
            command.extend(argv);
            output_with_timeout(&command, PROBE_TIMEOUT)
                .filter(|(ok, _)| *ok)
                .map(|(_, stdout)| parse_container_list(backend, &stdout))
        });
        let labelled = crate::sandbox_manage::gc_labelled_list_argv(backend).and_then(|argv| {
            let mut command = backend_prefix(backend);
            command.extend(argv);
            output_with_timeout(&command, PROBE_TIMEOUT)
                .filter(|(ok, _)| *ok)
                .map(|(_, stdout)| parse_container_list(backend, &stdout))
        });
        let plan = crate::sandbox_manage::plan_gc(
            db_worktrees,
            &containers,
            if labelled.is_some() {
                mine.as_deref()
            } else {
                None
            },
            labelled.as_deref(),
        );
        let plan = if labelled.is_some() {
            crate::sandbox_manage::refine_gc_plan(plan, db_worktrees, &mut |name| {
                let mut command = backend_prefix(backend);
                command.extend(crate::sandbox_manage::gc_label_probe_argv(name));
                output_with_timeout(&command, PROBE_TIMEOUT)
                    .filter(|(ok, _)| *ok)
                    .map(|(_, stdout)| {
                        let mut lines = stdout.lines();
                        (
                            lines.next().unwrap_or_default().to_string(),
                            lines.next().unwrap_or_default().to_string(),
                        )
                    })
            })
        } else {
            plan
        };

        for orphan in &plan.remove {
            removed_names.push(orphan.clone());
            let mut rm = backend_prefix(backend);
            rm.extend(["rm".into(), "-f".into(), orphan.clone()]);
            let _ = status_with_timeout(&rm, PROBE_TIMEOUT); // best-effort: teardown: orphan container rm -f is best-effort
        }
        if !plan.remove.is_empty() {
            removed_report.push((backend.label(), plan.remove));
        }
        if !plan.unverified.is_empty() {
            unverified_report.push((backend.label(), plan.unverified));
        }
    }
    let swept_vpn_staging = sweep_orphan_vpn_staging_in(
        &state_home,
        std::path::Path::new("thegn/vpn"),
        listed_any.then_some(&listed_names),
        &removed_names,
        SystemTime::now(),
        SECRET_ENV_FILE_MAX_AGE,
    );
    if swept_vpn_staging > 0 {
        tracing::debug!(
            target: "thegn::sandbox",
            removed = swept_vpn_staging,
            "swept orphan VPN staging dirs"
        );
    }
    GcReport {
        removed: removed_report,
        unverified: unverified_report,
    }
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
        let _ = status_with_timeout(&a, PROBE_TIMEOUT); // best-effort: teardown: orphan container rm -f is best-effort
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
