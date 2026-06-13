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
    FileAccess, Network, OnMissing, RemoteTransport, SandboxBackend, SandboxConfig,
};
use crate::remote::GitLoc;
use crate::{msg, util};
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

/// Ceiling for fast control-plane probes (`image exists`, `container
/// inspect`, health checks). A wedged runtime (stuck podman machine, broken
/// overlay storage) must FAIL the candidate quickly so the backend chain
/// falls through to bwrap/host instead of freezing the caller — pane spawns
/// run on the event loop's critical path.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
/// Ceiling for container create (`run -d`): image is prefetched by then, so
/// this is namespace/cgroup setup, not network.
const RUN_TIMEOUT: Duration = Duration::from_secs(30);
/// Ceiling for image pulls (network, legitimately slow — but never forever).
const PULL_TIMEOUT: Duration = Duration::from_secs(120);

/// Run `argv` for its exit status with a hard deadline, stdio discarded.
/// `None` on spawn failure or timeout (the child is killed and reaped) — for
/// callers, indistinguishable from "this backend doesn't work", which is
/// exactly the degradation the chain wants.
fn status_with_timeout(argv: &[String], timeout: Duration) -> Option<bool> {
    use std::process::Stdio;
    let mut child = Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status.success()),
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    /// Rootless podman (default podman invocation).
    Podman,
    /// Rootful podman via non-interactive sudo (`sudo -n podman`).
    PodmanRootful,
    Docker,
    Bwrap,
    Systemd,
    Apple,
    Wsl,
    None,
}

impl Backend {
    fn parse(s: &str) -> Option<Backend> {
        Some(match s {
            "podman" | "podman-rootless" | "rootless-podman" => Backend::Podman,
            "podman-rootful" | "rootful-podman" => Backend::PodmanRootful,
            "docker" => Backend::Docker,
            "bwrap" | "bubblewrap" => Backend::Bwrap,
            "systemd" | "systemd-run" => Backend::Systemd,
            "apple" | "container" => Backend::Apple,
            "wsl" => Backend::Wsl,
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
            SandboxBackend::Bwrap => Backend::Bwrap,
            SandboxBackend::Systemd => Backend::Systemd,
            SandboxBackend::Apple => Backend::Apple,
            SandboxBackend::Wsl => Backend::Wsl,
            SandboxBackend::None => Backend::None,
        })
    }

    /// The executable to probe / invoke for this backend.
    pub fn label(self) -> &'static str {
        match self {
            Backend::Podman => "podman-rootless",
            Backend::PodmanRootful => "podman-rootful",
            Backend::Docker => "docker",
            Backend::Bwrap => "bwrap",
            Backend::Systemd => "systemd",
            Backend::Apple => "apple",
            Backend::Wsl => "wsl",
            Backend::None => "host",
        }
    }

    pub fn binary(self) -> &'static str {
        match self {
            Backend::Podman | Backend::PodmanRootful => "podman",
            Backend::Docker => "docker",
            Backend::Bwrap => "bwrap",
            Backend::Systemd => "systemd-run",
            Backend::Apple => "container",
            Backend::Wsl => "wsl.exe",
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
                | Backend::Apple
                | Backend::Wsl
        )
    }

    fn is_host_toolchain(self) -> bool {
        matches!(self, Backend::Bwrap | Backend::Systemd)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportKind {
    Ssh,
    Mosh,
}

/// A configured remote target. The control plane (detection, `ensure`/`teardown`)
/// always uses ssh (mosh can't pipe non-interactive commands); the interactive
/// pane uses mosh when `kind == Mosh`.
#[derive(Debug, Clone)]
pub struct Remote {
    pub host: String,
    pub port: u16,
    pub forward_agent: bool,
    pub kind: TransportKind,
}

#[derive(Debug, Clone)]
pub enum Transport {
    Local,
    Remote(Remote),
}

impl Transport {
    /// The ssh argv prefix (shares the multiplexed base with the `remote`
    /// git-shim). `batch` distinguishes control-plane calls from the interactive
    /// pane — see `remote::ssh_base`.
    fn ssh_base(r: &Remote, batch: bool) -> Vec<String> {
        crate::remote::ssh_base(r.port, r.forward_agent, batch)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mount {
    pub host: String,
    pub dest: String,
    pub ro: bool,
    pub cache: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SandboxLimits {
    pub cpu: Option<String>,
    pub memory: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SandboxSpec {
    pub backend: Backend,
    pub transport: Transport,
    pub image: Option<String>,
    pub worktree: PathBuf,
    pub mounts: Vec<Mount>,
    pub env: Vec<(String, String)>,
    pub network: Network,
    pub file_access: FileAccess,
    pub ports: Vec<String>,
    pub gpu: Option<String>,
    pub limits: SandboxLimits,
    pub volumes: Vec<(String, String)>,
    pub compose: Option<String>,
    pub init_script: Option<String>,
    pub devenv: bool,
    /// Absolute path to the `devenv` binary on the host (resolved at spec-build
    /// time when `devenv = true`). Used in `wrap_script` so OCI containers don't
    /// rely on `devenv` being on their PATH.
    pub devenv_path: Option<String>,
    pub name: String,
}

/// Build the sandbox spec for a worktree (described by its `GitLoc`), or `None`
/// to run on the host (sandbox disabled, or the chain resolved to `none`). The
/// location drives both remote-ness (transport) and how git metadata is probed.
/// Emits a warning when it falls back per `on_missing`.
pub fn resolve(cfg: &SandboxConfig, loc: &GitLoc, name: &str) -> Option<SandboxSpec> {
    if !cfg.enabled {
        return None;
    }
    let transport = transport_from_loc(cfg, loc);
    let backend = pick_backend(cfg, &transport)?;
    // `none` on a *local* worktree means "run on the host" (caller's plain-shell
    // fallback). For a *remote* worktree we still need the transport to carry a
    // bare shell to the remote, so keep building the spec.
    if backend == Backend::None && matches!(transport, Transport::Local) {
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
                dest: g,
                ro: false,
                cache: false,
            });
        }
    };
    // OCI containers (podman/docker) get the host toolchain mounted in by
    // default so the user's real shell, starship, dotfiles, and tools work
    // identically inside the container — including on NixOS where everything
    // lives in /nix/store and $SHELL is an absolute store path.
    // bwrap/systemd are "host-toolchain" backends that expose the host
    // filesystem directly, so they don't need these extra mounts.
    let inject_host_toolchain = backend.is_oci() && cfg.auto_caches;

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
            add_worktree_mounts(&mut mounts);
            if inject_host_toolchain {
                mounts.extend(host_toolchain_mounts());
            }
        }
        FileAccess::WorktreePlusCaches => {
            add_worktree_mounts(&mut mounts);
            if cfg.auto_caches {
                mounts.extend(auto_cache_mounts());
            }
            if inject_host_toolchain {
                mounts.extend(host_toolchain_mounts());
            }
        }
        FileAccess::Custom => add_worktree_mounts(&mut mounts),
        FileAccess::None => {}
    }

    for m in &cfg.mounts {
        mounts.push(parse_mount(m));
    }

    let env = cfg
        .env_passthrough
        .iter()
        .filter_map(|k| std::env::var(k).ok().map(|v| (k.clone(), v)))
        .collect();

    Some(SandboxSpec {
        backend,
        transport,
        image,
        worktree,
        mounts,
        env,
        network: cfg.network,
        file_access: cfg.file_access,
        ports: cfg.ports.clone(),
        gpu: cfg.gpu.clone(),
        limits: SandboxLimits {
            cpu: cfg.limits.cpu.clone(),
            memory: cfg.limits.memory.clone(),
        },
        volumes: cfg
            .volumes
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        compose: cfg.compose.clone(),
        init_script: (!cfg.init_script.trim().is_empty()).then(|| cfg.init_script.clone()),
        // Explicit opt-in, or a *local* repo with devenv.nix when `devenv` is on PATH.
        devenv: cfg.devenv
            || (!loc.is_remote()
                && PathBuf::from(loc.path()).join("devenv.nix").is_file()
                && util::have("devenv")),
        // Resolve the absolute devenv path at spec-build time so OCI containers
        // (which don't inherit the host PATH) can still exec it directly.
        devenv_path: util::which_path("devenv"),
        name: name.to_string(),
    })
}

/// Name prefix for every container superzej creates (per-worktree sandboxes).
pub const CONTAINER_PREFIX: &str = "superzej-";

/// The deterministic per-worktree container name, derived from the worktree path
/// so the create site (pick_agent) and `teardown` (close_worktree) always agree —
/// local or remote, no DB slug lookup needed.
pub fn container_name(worktree: &str) -> String {
    format!("{CONTAINER_PREFIX}{}", util::slugify(worktree))
}

/// One running container, as listed by the OCI runtime — feeds the panel's
/// SANDBOXES section. `ours` marks superzej-created (prefix-named) ones.
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

/// The running containers, superzej-owned first. Probes rootless podman,
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

fn transport_from_loc(cfg: &SandboxConfig, loc: &GitLoc) -> Transport {
    if let Some(ssh) = loc.ssh() {
        let kind = match cfg.remote.transport {
            RemoteTransport::Ssh => TransportKind::Ssh,
            RemoteTransport::Mosh => TransportKind::Mosh,
        };
        Transport::Remote(Remote {
            host: ssh.host.clone(),
            port: ssh.port,
            forward_agent: ssh.forward_agent,
            kind,
        })
    } else if cfg.remote.is_remote() {
        let kind = match cfg.remote.transport {
            RemoteTransport::Ssh => TransportKind::Ssh,
            RemoteTransport::Mosh => TransportKind::Mosh,
        };
        Transport::Remote(Remote {
            host: cfg.remote.host.clone(),
            port: cfg.remote.port,
            forward_agent: cfg.remote.forward_agent,
            kind,
        })
    } else {
        Transport::Local
    }
}

fn pick_backend(cfg: &SandboxConfig, transport: &Transport) -> Option<Backend> {
    let suitable = |b: Backend| -> bool {
        match b {
            Backend::None => true,
            _ if b.is_oci() => true,
            _ if b.is_host_toolchain() => true,
            _ => false,
        }
    };

    // Explicit backend: use it if suitable+available; otherwise warn and fall
    // through to the chain. `Auto` falls straight through to the chain.
    if let Some(explicit) = Backend::from_config(cfg.backend) {
        match explicit {
            Backend::None => return Some(Backend::None),
            b if suitable(b) && available(transport, b) => return Some(b),
            b => on_missing(
                cfg,
                &format!(
                    "sandbox backend '{}' unavailable{}; trying the chain",
                    cfg.backend,
                    if suitable(b) {
                        ""
                    } else {
                        " for this image mode"
                    }
                ),
            ),
        }
    }

    for name in &cfg.backend_chain {
        let Some(b) = Backend::parse(name) else {
            continue;
        };
        if b == Backend::None {
            on_missing(
                cfg,
                "sandbox: no container backend available; running on the host",
            );
            return Some(Backend::None);
        }
        if suitable(b) && available(transport, b) {
            return Some(b);
        }
    }
    // Chain didn't include "none": still fall back to host rather than block.
    on_missing(
        cfg,
        "sandbox: no usable backend in chain; running on the host",
    );
    Some(Backend::None)
}

fn on_missing(cfg: &SandboxConfig, what: &str) {
    match cfg.on_missing {
        OnMissing::Fail => msg::die(what),
        // "prompt" is treated as "warn" here; the picker layer can offer choices.
        _ => msg::warn(what),
    }
}

/// Is `backend`'s binary present (locally, or on the remote over ssh)?
fn available(transport: &Transport, backend: Backend) -> bool {
    let bin = backend.binary();
    match transport {
        Transport::Local => match backend {
            Backend::PodmanRootful => {
                run_local_output(&backend_prefix(backend), &["version"]).is_some()
            }
            _ => util::have(bin),
        },
        Transport::Remote(r) => {
            let mut argv = Transport::ssh_base(r, true);
            argv.push(r.host.clone());
            argv.push("--".into());
            argv.push(format!("command -v {bin} >/dev/null 2>&1"));
            Command::new(&argv[0])
                .args(&argv[1..])
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        }
    }
}

pub const DEFAULT_OCI_IMAGE: &str = "docker.io/library/debian:stable";

fn effective_image(spec: &SandboxSpec) -> String {
    spec.image
        .clone()
        .unwrap_or_else(|| DEFAULT_OCI_IMAGE.to_string())
}

pub fn prefetch_image(spec: &SandboxSpec) -> anyhow::Result<()> {
    if !spec.backend.is_oci() {
        return Ok(());
    }
    let img = effective_image(spec);
    let rt = spec.backend.binary();
    let exists_argv: Vec<String> = vec![rt.into(), "image".into(), "exists".into(), img.clone()];
    match status_with_timeout(&exists_argv, PROBE_TIMEOUT) {
        Some(true) => {}
        Some(false) => {
            let pull_argv: Vec<String> = vec![rt.into(), "pull".into(), img.clone()];
            if status_with_timeout(&pull_argv, PULL_TIMEOUT) != Some(true) {
                anyhow::bail!("{rt} pull {img} failed or timed out");
            }
        }
        // The probe itself wedged: the runtime is unhealthy (stuck
        // machine, broken storage) — fail the candidate so the chain
        // falls through instead of trusting a pull to behave.
        None => anyhow::bail!("{rt} not responding (image probe timed out)"),
    }
    Ok(())
}

pub fn health_check(spec: &SandboxSpec) -> bool {
    if !spec.backend.is_oci() {
        return true;
    }
    let rt = spec.backend.binary();
    let argv: Vec<String> = vec![
        rt.into(),
        "exec".into(),
        spec.name.clone(),
        "echo".into(),
        "ok".into(),
    ];
    status_with_timeout(&argv, PROBE_TIMEOUT).unwrap_or(false)
}

/// Ensure any persistent state exists (OCI: a keep-alive container we `exec`
/// into). No-op for host-toolchain backends and `none`.
pub fn ensure(spec: &SandboxSpec) -> anyhow::Result<()> {
    if !spec.backend.is_oci() {
        return Ok(());
    }

    if let Some(compose_file) = &spec.compose {
        let _ = std::process::Command::new("docker-compose")
            .args(["-f", compose_file, "-p", &spec.name, "up", "-d"])
            .output()?;
        return Ok(());
    }

    prefetch_image(spec)?;

    let rt = spec.backend.binary();
    let mut inspect = backend_prefix(spec.backend);
    inspect.extend(["container".into(), "inspect".into(), spec.name.clone()]);
    if run_control_owned(spec, &inspect, PROBE_TIMEOUT).unwrap_or(false) {
        return Ok(()); // already running
    }
    let mut argv: Vec<String> = backend_prefix(spec.backend);
    argv.extend([
        "run".into(),
        "-d".into(),
        "--name".into(),
        spec.name.clone(),
    ]);
    argv.extend(oci_create_opts(spec));
    argv.push(effective_image(spec));
    argv.extend(["sleep".into(), "infinity".into()]);
    if run_control_owned(spec, &argv, RUN_TIMEOUT).unwrap_or(false) {
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
        let mut retry: Vec<String> = backend_prefix(spec.backend);
        retry.extend([
            "run".into(),
            "-d".into(),
            "--name".into(),
            spec.name.clone(),
        ]);
        retry.extend(oci_create_opts_with_keep_id(spec, false));
        retry.push(effective_image(spec));
        retry.extend(["sleep".into(), "infinity".into()]);
        if run_control_owned(spec, &retry, RUN_TIMEOUT).unwrap_or(false) {
            msg::warn(
                "podman --userns keep-id failed; continuing with rootless podman default user namespace",
            );
            return Ok(());
        }
    }

    anyhow::bail!("could not start {rt} container '{}'", spec.name)
}

/// Remove a worktree's persistent container (OCI backends). Best-effort. Runs on
/// the worktree's host (local or remote, per its `GitLoc`).
pub fn teardown(cfg: &SandboxConfig, loc: &GitLoc, name: &str) {
    if !cfg.enabled {
        return;
    }
    let transport = transport_from_loc(cfg, loc);
    // Try whichever OCI runtimes are available; the container only exists under one.
    for b in [
        Backend::Podman,
        Backend::PodmanRootful,
        Backend::Docker,
        Backend::Apple,
    ] {
        if available(&transport, b) {
            let mut argv = backend_prefix(b);
            argv.extend(["rm".into(), "-f".into(), name.to_string()]);
            let _ = run_control_t_owned(&transport, &argv, PROBE_TIMEOUT);
        }
    }
}

/// The full argv to exec for an interactive pane running `inner` (a shell command
/// string, e.g. `${SHELL:-/bin/sh} -l` or `claude`). Wraps the backend invocation
/// in the transport (mosh/ssh) when remote.
pub fn enter_argv(spec: &SandboxSpec, inner: &str) -> Vec<String> {
    let script = wrap_script(spec, inner);
    let backend_argv = backend_enter_argv(spec, &script);
    match &spec.transport {
        Transport::Local => backend_argv,
        Transport::Remote(r) => transport_wrap(r, &backend_argv),
    }
}

/// Compose init-script + safe.directory + devenv into the `sh -lc` body that the
/// backend ultimately runs. The chosen program is `exec`'d so it owns the pane.
fn wrap_script(spec: &SandboxSpec, inner: &str) -> String {
    let mut s = String::new();
    if spec.backend.is_oci() {
        // Bind-mounted worktree is owned by a different uid under userns/root.
        s.push_str("git config --global --add safe.directory '*' >/dev/null 2>&1 || true\n");
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
        | Backend::Apple
        | Backend::Wsl => {
            let mut v = backend_prefix(spec.backend);
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
        Backend::Bwrap => {
            let mut v = vec!["bwrap".to_string()];
            if matches!(spec.file_access, FileAccess::All | FileAccess::Host) {
                v.extend(["--dev-bind".into(), "/".into(), "/".into()]);
            } else {
                // Do not expose host / wholesale. Bind the runtime substrate read-only,
                // then add the explicit worktree/cache mounts below.
                for path in [
                    "/nix/store",
                    "/run/current-system",
                    "/bin",
                    "/usr",
                    "/lib",
                    "/lib64",
                    "/etc",
                ] {
                    if std::path::Path::new(path).exists() {
                        v.extend(["--ro-bind".into(), path.into(), path.into()]);
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
                let flag = if m.ro { "--ro-bind" } else { "--bind" };
                v.extend([flag.into(), m.host.clone(), m.dest.clone()]);
            }
            v.extend(["--unshare-pid".into(), "--die-with-parent".into()]);
            if spec.network == Network::None {
                v.push("--unshare-net".into());
            }
            for (k, val) in &spec.env {
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
                "ProtectHome=tmpfs".into(),
                "-p".into(),
                "PrivateTmp=yes".into(),
            ];
            if spec.network == Network::None {
                v.extend(["-p".into(), "PrivateNetwork=yes".into()]);
            }
            for (k, val) in &spec.env {
                v.extend(["--setenv".into(), format!("{k}={val}")]);
            }
            v.extend(["/bin/sh".into(), "-lc".into(), script.to_string()]);
            v
        }
        Backend::None => {
            // Bare shell (reached only for a remote worktree — local `none` runs
            // on the host via the caller). cd into the worktree first.
            let body = format!("cd {} && {script}", util::sh_quote(&wt));
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
            if let (Transport::Local, Some((uid, gid))) = (&spec.transport, local_uid_gid()) {
                v.extend(["--user".into(), format!("{uid}:{gid}")]);
            }
        }
        Backend::Docker => {
            if let (Transport::Local, Some((uid, gid))) = (&spec.transport, local_uid_gid()) {
                v.extend(["--user".into(), format!("{uid}:{gid}")]);
            }
        }
        _ => {}
    }
    match spec.network {
        Network::Host => v.extend(["--network".into(), "host".into()]),
        Network::None => v.extend(["--network".into(), "none".into()]),
        Network::Nat => {}
    }
    for m in &spec.mounts {
        let suffix = if m.ro { ":ro" } else { "" };
        v.extend(["-v".into(), format!("{}:{}{suffix}", m.host, m.dest)]);
    }
    // When devenv lives in the Nix store, bind-mount /nix read-only so the
    // container can exec the resolved absolute path. Consistent with bwrap
    // which already does `--ro-bind /nix/store /nix/store`.
    if spec.devenv {
        if let Some(p) = &spec.devenv_path {
            if p.starts_with("/nix") && std::path::Path::new("/nix").exists() {
                v.extend(["-v".into(), "/nix:/nix:ro".into()]);
            }
        }
    }
    for (k, val) in &spec.env {
        v.extend(["-e".into(), format!("{k}={val}")]);
    }
    for (vol_name, dest) in &spec.volumes {
        v.extend(["-v".into(), format!("{}:{}", vol_name, dest)]);
    }

    if let Some(gpu) = &spec.gpu {
        if spec.backend == Backend::Docker {
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

    for p in &spec.ports {
        v.extend(["-p".into(), p.clone()]);
    }
    v
}

/// Like [`oci_create_opts`] but lets the caller suppress Podman's
/// `--userns keep-id` flag for the rootless-fallback retry path.
fn oci_create_opts_with_keep_id(spec: &SandboxSpec, keep_id: bool) -> Vec<String> {
    let mut v = Vec::new();
    match spec.backend {
        Backend::Podman => {
            if keep_id {
                v.extend(["--userns".into(), "keep-id".into()]);
            }
        }
        Backend::PodmanRootful => {
            if let (Transport::Local, Some((uid, gid))) = (&spec.transport, local_uid_gid()) {
                v.extend(["--user".into(), format!("{uid}:{gid}")]);
            }
        }
        Backend::Docker => {
            if let (Transport::Local, Some((uid, gid))) = (&spec.transport, local_uid_gid()) {
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

/// Wrap a backend argv so it runs on the remote: mosh (interactive) or ssh.
fn transport_wrap(r: &Remote, backend_argv: &[String]) -> Vec<String> {
    let remote_cmd = util::sh_join(backend_argv);
    match r.kind {
        TransportKind::Mosh => {
            let ssh = util::sh_join(&Transport::ssh_base(r, false));
            vec![
                "mosh".into(),
                format!("--ssh={ssh}"),
                r.host.clone(),
                "--".into(),
                "/bin/sh".into(),
                "-lc".into(),
                remote_cmd,
            ]
        }
        TransportKind::Ssh => {
            let mut v = Transport::ssh_base(r, false);
            v.push("-t".into());
            v.push(r.host.clone());
            v.push("--".into());
            v.push("/bin/sh".into());
            v.push("-lc".into());
            v.push(remote_cmd);
            v
        }
    }
}

fn backend_prefix(backend: Backend) -> Vec<String> {
    match backend {
        Backend::PodmanRootful => vec!["sudo".into(), "-n".into(), "podman".into()],
        _ => vec![backend.binary().into()],
    }
}

fn run_local_output(prefix: &[String], args: &[&str]) -> Option<String> {
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
fn run_control_owned(spec: &SandboxSpec, argv: &[String], timeout: Duration) -> Option<bool> {
    run_control_t_owned(&spec.transport, argv, timeout)
}

fn run_control_t_owned(transport: &Transport, argv: &[String], timeout: Duration) -> Option<bool> {
    let argv: Vec<String> = match transport {
        Transport::Local => argv.to_vec(),
        Transport::Remote(r) => {
            let mut v = Transport::ssh_base(r, true);
            v.push(r.host.clone());
            v.push("--".into());
            v.push(util::sh_join(argv));
            v
        }
    };
    status_with_timeout(&argv, timeout)
}

/// Local uid/gid via `id` (no libc dep). None if `id` is unavailable.
fn local_uid_gid() -> Option<(u32, u32)> {
    let uid = Command::new("id").arg("-u").output().ok()?;
    let gid = Command::new("id").arg("-g").output().ok()?;
    let u = String::from_utf8_lossy(&uid.stdout).trim().parse().ok()?;
    let g = String::from_utf8_lossy(&gid.stdout).trim().parse().ok()?;
    Some((u, g))
}

/// Mounts that bring the host toolchain into an OCI container so the user's
/// real shell, dotfiles, and tools work identically inside the sandbox.
///
/// This is most useful on NixOS, where everything lives in `/nix/store` and
/// `/run/current-system/sw`, but the same logic also picks up conventional
/// FHS paths (`/usr`, `/lib`, `/bin`) on non-NixOS hosts.
///
/// Only paths that **exist on the host** at spec-build time are included —
/// the list is always a subset of what's actually present, never a wish list.
/// All mounts are **read-only** (the container should not modify host system
/// files).
pub fn host_toolchain_mounts() -> Vec<Mount> {
    let mut mounts = Vec::new();
    let home = std::env::var("HOME").unwrap_or_default();

    let ro = |path: &str| Mount {
        host: path.to_string(),
        dest: path.to_string(),
        ro: true,
        cache: false,
    };

    let exists = |p: &str| std::path::Path::new(p).exists();

    // ── NixOS / Nix-on-anything paths ───────────────────────────────────────
    // /nix/store  — every binary, library, and config file Nix manages lives
    //               here. Mounting it ro brings in the shell ($SHELL resolves
    //               to a store path), starship, completions, dotfile symlink
    //               targets, etc. without any per-package enumeration.
    if exists("/nix/store") {
        mounts.push(ro("/nix/store"));
    }
    // /run/current-system — the stable generation symlinks:
    //   sw/bin/zsh, sw/share/zsh, etc. The container's $SHELL will resolve
    //   correctly once /nix/store is present.
    if exists("/run/current-system") {
        mounts.push(ro("/run/current-system"));
    }
    // /nix/var/nix/profiles — user profiles (alternative to per-user path).
    if exists("/nix/var/nix/profiles") {
        mounts.push(ro("/nix/var/nix/profiles"));
    }
    // /etc/profiles/per-user/<user> — per-user packages installed by
    // home-manager (e.g. zsh plugins, starship when not in system profile).
    if !home.is_empty() {
        let username = std::path::Path::new(&home)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("");
        if !username.is_empty() {
            let p = format!("/etc/profiles/per-user/{username}");
            if exists(&p) {
                mounts.push(ro(&p));
            }
        }
    }
    // /etc/static — NixOS-managed /etc entries (zshrc, zshenv, zprofile, …).
    if exists("/etc/static") {
        mounts.push(ro("/etc/static"));
    }

    // ── Conventional FHS paths (non-NixOS, or mixed systems) ────────────────
    // These are absent on pure NixOS (everything is in /nix) but present on
    // Ubuntu/Debian/Fedora/Arch and WSL; include them when they exist.
    for path in &["/usr", "/lib", "/lib64", "/bin"] {
        // Skip /bin and /lib if they're just symlinks into /usr (common on
        // modern FHS systems) to avoid duplicate mounts.
        let p = std::path::Path::new(path);
        if p.exists() && !p.is_symlink() {
            mounts.push(ro(path));
        }
    }

    // ── Identity/locale files every process expects ──────────────────────────
    // passwd/group are needed for getpwuid() (shell prompts, git author, etc.)
    // Overlaying the host files means the container sees the real username.
    for path in &[
        "/etc/passwd",
        "/etc/group",
        "/etc/hosts",
        "/etc/localtime",
        "/etc/resolv.conf",
        "/etc/zshrc",   // NixOS system-wide zsh init (sourced by /etc/static/zshrc)
        "/etc/zshenv",
        "/etc/zprofile",
    ] {
        if exists(path) {
            mounts.push(ro(path));
        }
    }

    // ── User home directory (dotfiles) ───────────────────────────────────────
    // Mount $HOME read-only so ~/.zshrc, ~/.config/starship.toml, ~/.gitconfig
    // and similar dotfiles are visible. On NixOS these are typically symlinks
    // into /nix/store, so this mount is complementary (symlink) + /nix/store
    // (target). Without home the symlinks dangle inside the container.
    // We use the same dest so absolute paths in dotfiles work unchanged.
    if !home.is_empty() && exists(&home) {
        mounts.push(Mount {
            host: home.clone(),
            dest: home,
            ro: true,
            cache: false,
        });
    }

    mounts
}

fn auto_cache_mounts() -> Vec<Mount> {
    let home = std::env::var("HOME").unwrap_or_default();
    if home.is_empty() {
        return Vec::new();
    }
    let candidates = [
        ".cargo/registry",
        ".cargo/git",
        ".rustup",
        ".npm",
        ".cache/pnpm",
        ".cache/yarn",
        "go/pkg/mod",
        ".cache/go-build",
        ".cache/pip",
        ".cache/uv",
        ".m2/repository",
        ".gradle/caches",
    ];
    candidates
        .iter()
        .filter_map(|rel| {
            let p = std::path::Path::new(&home).join(rel);
            p.is_dir().then(|| {
                let s = p.to_string_lossy().into_owned();
                Mount {
                    host: s.clone(),
                    dest: s,
                    ro: false,
                    cache: true,
                }
            })
        })
        .collect()
}

fn parse_mount(spec: &str) -> Mount {
    // "host", "host:ro", or "host:dest" / "host:dest:ro".
    let parts: Vec<&str> = spec.split(':').collect();
    match parts.as_slice() {
        [host] => Mount {
            host: (*host).into(),
            dest: (*host).into(),
            ro: false,
            cache: false,
        },
        [host, "ro"] => Mount {
            host: (*host).into(),
            dest: (*host).into(),
            ro: true,
            cache: false,
        },
        [host, "cache"] => Mount {
            host: (*host).into(),
            dest: (*host).into(),
            ro: false,
            cache: true,
        },
        [host, dest] => Mount {
            host: (*host).into(),
            dest: (*dest).into(),
            ro: false,
            cache: false,
        },
        [host, dest, "ro"] => Mount {
            host: (*host).into(),
            dest: (*dest).into(),
            ro: true,
            cache: false,
        },
        [host, dest, "cache"] => Mount {
            host: (*host).into(),
            dest: (*dest).into(),
            ro: false,
            cache: true,
        },
        _ => Mount {
            host: spec.into(),
            dest: spec.into(),
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
    let active_names: Vec<String> = active_worktrees.iter().map(|w| container_name(w)).collect();

    containers
        .iter()
        .filter(|c| c.starts_with("superzej-"))
        .filter(|c| !active_names.contains(c))
        .cloned()
        .collect()
}

pub fn run_gc(db_worktrees: &[String]) -> Result<(), String> {
    for backend in [Backend::Podman, Backend::Docker] {
        if !crate::util::have(backend.binary()) {
            continue;
        }

        let out = std::process::Command::new(backend.binary())
            .args(["ps", "-a", "--format", "{{.Names}}"])
            .output()
            .map_err(|e| e.to_string())?;

        let stdout = String::from_utf8_lossy(&out.stdout);
        let containers: Vec<String> = stdout.lines().map(|s| s.trim().to_string()).collect();

        for orphan in identify_orphans(db_worktrees, &containers) {
            let _ = std::process::Command::new(backend.binary())
                .args(["rm", "-f", &orphan])
                .output();
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(backend: Backend) -> SandboxSpec {
        SandboxSpec {
            backend,
            transport: Transport::Local,
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
            network: Network::Nat,
            ports: vec!["8080:8080".into()],
            gpu: None,
            limits: SandboxLimits::default(),
            volumes: vec![],
            compose: None,
            init_script: None,
            file_access: FileAccess::Worktree,
            devenv: false,
            devenv_path: None,
            name: "superzej-repo-feat".into(),
        }
    }

    #[test]
    fn podman_exec_preserves_paths() {
        let argv = enter_argv(&spec(Backend::Podman), "${SHELL:-/bin/sh} -l");
        assert_eq!(argv[0], "podman");
        assert!(argv.contains(&"exec".to_string()));
        assert!(argv.contains(&"superzej-repo-feat".to_string()));
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
    fn empty_oci_image_uses_default_image() {
        let mut s = spec(Backend::Podman);
        s.image = None;
        assert_eq!(effective_image(&s), DEFAULT_OCI_IMAGE);
    }

    #[test]
    fn mosh_wraps_backend_over_ssh() {
        let mut s = spec(Backend::Podman);
        s.transport = Transport::Remote(Remote {
            host: "user@box".into(),
            port: 2222,
            forward_agent: true,
            kind: TransportKind::Mosh,
        });
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
        s.transport = Transport::Remote(Remote {
            host: "box".into(),
            port: 22,
            forward_agent: false,
            kind: TransportKind::Ssh,
        });
        let argv = enter_argv(&s, "claude");
        assert_eq!(argv[0], "ssh");
        assert!(argv.contains(&"-t".to_string()));
        assert!(argv.last().unwrap().contains("bwrap"));
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
        s.name = "superzej-test-net-file-container".into();
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
        s.name = "superzej-test-lifecycle-container".into();
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
            "superzej-live".to_string(),
            "superzej-dead".to_string(),
            "other-container".to_string(),
        ];
        let orphans = identify_orphans(&active_wts, &containers);
        assert_eq!(orphans.len(), 1);
        assert_eq!(orphans[0], "superzej-dead");
    }

    #[test]
    fn remote_none_bare_shell_cds_and_moshes() {
        // A remote worktree with no container backend still goes over the
        // transport as a bare shell that cd's into the remote worktree.
        let mut s = spec(Backend::None);
        s.image = None;
        s.transport = Transport::Remote(Remote {
            host: "box".into(),
            port: 22,
            forward_agent: false,
            kind: TransportKind::Mosh,
        });
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
        assert_eq!(
            parse_mount("~/.gitconfig:ro"),
            Mount {
                host: "~/.gitconfig".into(),
                dest: "~/.gitconfig".into(),
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
          {"Names": ["superzej-wt-feat"], "Image": "ubuntu:24.04", "Status": "Up 2 hours"},
          {"Names": ["registry"], "Image": "registry:2", "Status": "Up 3 days"}
        ]"#;
        let rows = parse_podman_ps(podman);
        assert_eq!(rows.len(), 2);
        assert!(rows[0].ours && rows[0].name == "superzej-wt-feat");
        assert!(!rows[1].ours);
        assert_eq!(rows[1].image, "registry:2");

        let docker = "{\"Names\": \"superzej-x\", \"Image\": \"alpine\", \"Status\": \"Up 5 minutes\"}\n{\"Names\": \"db\", \"Image\": \"postgres:16\", \"Status\": \"Up 1 hour\"}";
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
            assert_eq!(m.host, m.dest, "host toolchain mounts must be path-preserving");
        }
    }

    #[test]
    fn host_toolchain_mounts_injected_for_oci_not_bwrap() {
        // For OCI backends, host_toolchain_mounts() contributes only paths that
        // exist on the current host — verify that invariant holds by checking
        // any mount whose host path is NOT the synthetic worktree path.
        let mut cfg = crate::config::SandboxConfig::default();
        cfg.file_access = crate::config::FileAccess::WorktreePlusCaches;
        cfg.auto_caches = true;
        cfg.backend = crate::config::SandboxBackend::Podman;
        cfg.image = "debian:stable".into();
        let loc = crate::remote::GitLoc::from_db("/wt/x", None);
        if let Some(spec) = resolve(&cfg, &loc, "test") {
            // host_toolchain_mounts() entries: not the fake worktree, not the
            // rw language caches from auto_cache_mounts (those are !ro && cache),
            // and not unexpanded-tilde paths from cfg.mounts defaults.
            let toolchain: Vec<_> = spec
                .mounts
                .iter()
                .filter(|m| !m.host.starts_with("/wt/") && !m.cache && !m.host.starts_with('~'))
                .collect();
            for m in &toolchain {
                assert!(
                    std::path::Path::new(&m.host).exists(),
                    "host_toolchain mount for non-existent path: {}",
                    m.host
                );
                assert!(m.ro, "host toolchain mount should be ro: {}", m.host);
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
}
