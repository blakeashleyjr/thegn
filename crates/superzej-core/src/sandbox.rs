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

use crate::config::{Network, OnMissing, RemoteTransport, SandboxBackend, SandboxConfig};
use crate::remote::GitLoc;
use crate::{msg, util};
use std::path::PathBuf;
use std::process::Command;

/// Runtime backend (resolved from the config-facing [`SandboxBackend`]; this set
/// has no `Auto` — auto resolution is what produces a concrete `Backend`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Podman,
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
            "podman" => Backend::Podman,
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
    fn from_config(b: SandboxBackend) -> Option<Backend> {
        Some(match b {
            SandboxBackend::Auto => return None,
            SandboxBackend::Podman => Backend::Podman,
            SandboxBackend::Docker => Backend::Docker,
            SandboxBackend::Bwrap => Backend::Bwrap,
            SandboxBackend::Systemd => Backend::Systemd,
            SandboxBackend::Apple => Backend::Apple,
            SandboxBackend::Wsl => Backend::Wsl,
            SandboxBackend::None => Backend::None,
        })
    }

    /// The executable to probe / invoke for this backend.
    fn binary(self) -> &'static str {
        match self {
            Backend::Podman => "podman",
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
    fn is_oci(self) -> bool {
        matches!(
            self,
            Backend::Podman | Backend::Docker | Backend::Apple | Backend::Wsl
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
    pub init_script: Option<String>,
    pub devenv: bool,
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
        (!t.is_empty()).then(|| t.to_string())
    };
    let worktree = PathBuf::from(loc.path());
    // Path-preserving git mounts: the worktree and the repo's git-common dir
    // (probed via the location, so it's the *remote* path for remote worktrees).
    let git_common = loc
        .git_out(&["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .map(PathBuf::from)
        .filter(|p| p.as_path() != worktree && !worktree.starts_with(p));

    let mut mounts = vec![Mount {
        host: loc.path(),
        dest: loc.path(),
        ro: false,
    }];
    if let Some(gc) = &git_common {
        let g = gc.to_string_lossy().into_owned();
        mounts.push(Mount {
            host: g.clone(),
            dest: g,
            ro: false,
        });
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
        init_script: (!cfg.init_script.trim().is_empty()).then(|| cfg.init_script.clone()),
        // Explicit opt-in, or a *local* repo with devenv.nix when `devenv` is on PATH.
        devenv: cfg.devenv
            || (!loc.is_remote()
                && PathBuf::from(loc.path()).join("devenv.nix").is_file()
                && util::have("devenv")),
        name: name.to_string(),
    })
}

/// The deterministic per-worktree container name, derived from the worktree path
/// so the create site (pick_agent) and `teardown` (close_worktree) always agree —
/// local or remote, no DB slug lookup needed.
pub fn container_name(worktree: &str) -> String {
    format!("superzej-{}", util::slugify(worktree))
}

/// Transport for a worktree: remote when its location is remote (kind from the
/// configured `[sandbox.remote] transport`), else local.
fn transport_from_loc(cfg: &SandboxConfig, loc: &GitLoc) -> Transport {
    match loc.ssh() {
        None => Transport::Local,
        Some(ssh) => {
            let kind = if cfg.remote.transport == RemoteTransport::Ssh {
                TransportKind::Ssh
            } else {
                TransportKind::Mosh
            };
            Transport::Remote(Remote {
                host: ssh.host.clone(),
                port: ssh.port,
                forward_agent: ssh.forward_agent,
                kind,
            })
        }
    }
}

/// Choose a backend: honor an explicit choice when available, else walk the
/// chain. Image presence filters candidates (OCI runtimes need an image; bwrap/
/// systemd reuse the host toolchain). Always resolvable — the chain ends in
/// `none` (host).
fn pick_backend(cfg: &SandboxConfig, transport: &Transport) -> Option<Backend> {
    let image_set = !cfg.image.trim().is_empty();
    let suitable = |b: Backend| -> bool {
        match b {
            Backend::None => true,
            _ if b.is_oci() => image_set,
            _ if b.is_host_toolchain() => !image_set,
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
        Transport::Local => util::have(bin),
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

/// Ensure any persistent state exists (OCI: a keep-alive container we `exec`
/// into). No-op for host-toolchain backends and `none`.
pub fn ensure(spec: &SandboxSpec) -> anyhow::Result<()> {
    if !spec.backend.is_oci() {
        return Ok(());
    }
    let rt = spec.backend.binary();
    if run_control(spec, &[rt, "container", "inspect", &spec.name]).unwrap_or(false) {
        return Ok(()); // already running
    }
    let mut argv: Vec<String> = vec![
        rt.into(),
        "run".into(),
        "-d".into(),
        "--name".into(),
        spec.name.clone(),
    ];
    argv.extend(oci_create_opts(spec));
    argv.push(
        spec.image
            .clone()
            .unwrap_or_else(|| "docker.io/library/debian:stable".into()),
    );
    argv.extend(["sleep".into(), "infinity".into()]);
    if run_control(spec, &argv.iter().map(String::as_str).collect::<Vec<_>>()).unwrap_or(false) {
        Ok(())
    } else {
        anyhow::bail!("could not start {rt} container '{}'", spec.name)
    }
}

/// Remove a worktree's persistent container (OCI backends). Best-effort. Runs on
/// the worktree's host (local or remote, per its `GitLoc`).
pub fn teardown(cfg: &SandboxConfig, loc: &GitLoc, name: &str) {
    if !cfg.enabled {
        return;
    }
    let transport = transport_from_loc(cfg, loc);
    // Try whichever OCI runtimes are available; the container only exists under one.
    for b in [Backend::Podman, Backend::Docker, Backend::Apple] {
        if available(&transport, b) {
            let rt = b.binary();
            let _ = run_control_t(&transport, &[rt, "rm", "-f", name]);
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
        s.push_str(&format!("exec devenv shell -- {inner}"));
    } else {
        s.push_str(&format!("exec {inner}"));
    }
    s
}

/// The backend-specific argv that runs `/bin/sh -lc <script>` in the sandbox.
fn backend_enter_argv(spec: &SandboxSpec, script: &str) -> Vec<String> {
    let wt = spec.worktree.to_string_lossy().into_owned();
    match spec.backend {
        Backend::Podman | Backend::Docker | Backend::Apple | Backend::Wsl => {
            let rt = spec.backend.binary();
            let mut v = vec![
                rt.to_string(),
                "exec".into(),
                "-it".into(),
                "--workdir".into(),
                wt,
                spec.name.clone(),
                "/bin/sh".into(),
                "-lc".into(),
                script.to_string(),
            ];
            if spec.backend == Backend::Wsl {
                // Aspirational: shell out into WSL's distro to run podman there.
                v.insert(0, "wsl.exe".into());
                v.insert(1, "--".into());
            }
            v
        }
        Backend::Bwrap => {
            let mut v = vec!["bwrap".to_string()];
            // Share the host runtime read-only, then bind the writable worktree.
            v.extend(["--dev-bind".into(), "/".into(), "/".into()]);
            v.extend(["--chdir".into(), wt]);
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
    for (k, val) in &spec.env {
        v.extend(["-e".into(), format!("{k}={val}")]);
    }
    v
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

/// Run a control-plane command (locally, or on the remote over ssh). Returns
/// whether it succeeded.
fn run_control(spec: &SandboxSpec, argv: &[&str]) -> Option<bool> {
    run_control_t(&spec.transport, argv)
}

fn run_control_t(transport: &Transport, argv: &[&str]) -> Option<bool> {
    let argv: Vec<String> = match transport {
        Transport::Local => argv.iter().map(|s| s.to_string()).collect(),
        Transport::Remote(r) => {
            let owned: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
            let mut v = Transport::ssh_base(r, true);
            v.push(r.host.clone());
            v.push("--".into());
            v.push(util::sh_join(&owned));
            v
        }
    };
    Command::new(&argv[0])
        .args(&argv[1..])
        .output()
        .ok()
        .map(|o| o.status.success())
}

/// Local uid/gid via `id` (no libc dep). None if `id` is unavailable.
fn local_uid_gid() -> Option<(u32, u32)> {
    let uid = Command::new("id").arg("-u").output().ok()?;
    let gid = Command::new("id").arg("-g").output().ok()?;
    let u = String::from_utf8_lossy(&uid.stdout).trim().parse().ok()?;
    let g = String::from_utf8_lossy(&gid.stdout).trim().parse().ok()?;
    Some((u, g))
}

fn parse_mount(spec: &str) -> Mount {
    // "host", "host:ro", or "host:dest" / "host:dest:ro".
    let parts: Vec<&str> = spec.split(':').collect();
    match parts.as_slice() {
        [host] => Mount {
            host: (*host).into(),
            dest: (*host).into(),
            ro: false,
        },
        [host, "ro"] => Mount {
            host: (*host).into(),
            dest: (*host).into(),
            ro: true,
        },
        [host, dest] => Mount {
            host: (*host).into(),
            dest: (*dest).into(),
            ro: false,
        },
        [host, dest, "ro"] => Mount {
            host: (*host).into(),
            dest: (*dest).into(),
            ro: true,
        },
        _ => Mount {
            host: spec.into(),
            dest: spec.into(),
            ro: false,
        },
    }
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
                },
                Mount {
                    host: "/repo/.git".into(),
                    dest: "/repo/.git".into(),
                    ro: false,
                },
            ],
            env: vec![("GH_TOKEN".into(), "abc".into())],
            network: Network::Nat,
            init_script: None,
            devenv: false,
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
        let argv = enter_argv(&s, "claude");
        assert_eq!(argv[0], "bwrap");
        let joined = argv.join(" ");
        assert!(joined.contains("--bind /wt/feat /wt/feat"));
        assert!(joined.contains("--bind /repo/.git /repo/.git"));
        assert!(joined.contains("--chdir /wt/feat"));
        assert_eq!(argv.last().unwrap(), "exec claude");
    }

    #[test]
    fn oci_create_opts_map_userns_and_mounts() {
        let opts = oci_create_opts(&spec(Backend::Podman));
        let j = opts.join(" ");
        assert!(j.contains("--userns keep-id"));
        assert!(j.contains("-v /wt/feat:/wt/feat"));
        assert!(j.contains("-v /repo/.git:/repo/.git"));
        assert!(j.contains("-e GH_TOKEN=abc"));
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
                ro: true
            }
        );
        assert_eq!(
            parse_mount("/a:/b"),
            Mount {
                host: "/a".into(),
                dest: "/b".into(),
                ro: false
            }
        );
    }
}
