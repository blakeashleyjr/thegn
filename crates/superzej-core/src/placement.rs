//! Execution **placement** — *where* a worktree's processes run, decoupled from
//! *how* they're isolated (the sandbox [`Backend`](crate::sandbox::Backend)) and
//! *where* its files live ([`DataMode`](crate::env::DataMode)).
//!
//! This generalizes the old `Transport` (which only knew `Local | Remote(ssh)`):
//! a placement owns the **exec primitive** that wraps a backend's argv. Local is
//! a passthrough; SSH wraps with `ssh`/`mosh`; k8s wraps with `kubectl exec`; a
//! provider (Daytona, …) supplies an exec prefix resolved from its CLI/API.
//!
//! The composition in [`sandbox::enter_argv`](crate::sandbox::enter_argv) is:
//! `placement.interactive_argv( backend_enter_argv( wrap_script(inner) ) )` —
//! the inner backend wrap is unchanged; only the outer wrap is polymorphic.
//!
//! Invariant: for `Local` and a plain `Ssh` (no extra knobs) the emitted argv is
//! byte-identical to the pre-refactor `Transport` path, so the existing remote
//! argv tests hold unchanged.

use crate::remote::ssh_base;
use crate::util;

/// Interactive-pane transport for an SSH placement. The control plane always
/// uses plain ssh (mosh can't pipe non-interactive commands); only the pane
/// honours `Mosh`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportKind {
    Ssh,
    Mosh,
}

/// An SSH (or mosh-over-ssh) placement. The first four fields reproduce the old
/// `Remote`; the rest are per-environment SSH knobs (empty/`None` for a worktree
/// whose target was derived from its `GitLoc`, which keeps argv identical).
#[derive(Debug, Clone)]
pub struct SshPlacement {
    pub host: String,
    pub port: u16,
    pub forward_agent: bool,
    pub kind: TransportKind,
    /// `ssh -F <path>` — a dedicated ssh config file for this environment.
    pub ssh_config: Option<String>,
    /// `ssh -J <host>` — a ProxyJump bastion.
    pub jump_host: Option<String>,
    /// `ssh -i <path>` — an explicit identity file.
    pub identity: Option<String>,
    /// Extra raw ssh args appended verbatim (advanced escape hatch).
    pub extra_args: Vec<String>,
}

impl SshPlacement {
    /// A plain target (no extra knobs) — the shape produced from a `GitLoc`.
    pub fn plain(host: String, port: u16, forward_agent: bool, kind: TransportKind) -> Self {
        SshPlacement {
            host,
            port,
            forward_agent,
            kind,
            ssh_config: None,
            jump_host: None,
            identity: None,
            extra_args: Vec::new(),
        }
    }

    /// The multiplexed ssh argv prefix (without host). `batch` selects the
    /// control plane (BatchMode) vs the interactive pane — see [`ssh_base`].
    /// Extra knobs are appended only when set, so a plain target is unchanged.
    pub fn ssh_base(&self, batch: bool) -> Vec<String> {
        let mut v = ssh_base(self.port, self.forward_agent, batch);
        if let Some(cfg) = &self.ssh_config {
            v.push("-F".into());
            v.push(cfg.clone());
        }
        if let Some(j) = &self.jump_host {
            v.push("-J".into());
            v.push(j.clone());
        }
        if let Some(id) = &self.identity {
            v.push("-i".into());
            v.push(id.clone());
        }
        v.extend(self.extra_args.iter().cloned());
        v
    }

    /// Wrap a backend argv for the interactive pane (mosh or ssh -t).
    fn interactive_wrap(&self, backend_argv: &[String]) -> Vec<String> {
        let remote_cmd = util::sh_join(backend_argv);
        match self.kind {
            TransportKind::Mosh => {
                let ssh = util::sh_join(&self.ssh_base(false));
                vec![
                    "mosh".into(),
                    format!("--ssh={ssh}"),
                    self.host.clone(),
                    "--".into(),
                    "/bin/sh".into(),
                    "-lc".into(),
                    remote_cmd,
                ]
            }
            TransportKind::Ssh => {
                let mut v = self.ssh_base(false);
                v.push("-t".into());
                v.push(self.host.clone());
                v.push("--".into());
                v.push("/bin/sh".into());
                v.push("-lc".into());
                v.push(remote_cmd);
                v
            }
        }
    }

    /// Wrap an argv for a non-interactive control-plane call (always ssh batch).
    fn control_wrap(&self, argv: &[String]) -> Vec<String> {
        let mut v = self.ssh_base(true);
        v.push(self.host.clone());
        v.push("--".into());
        v.push(util::sh_join(argv));
        v
    }

    /// argv to sshfs-mount the host's `remote_path` at the local `mountpoint`
    /// (the `DataMode::Sshfs` data mode). `reconnect` + keepalive keep the mount
    /// alive across flaky links; the identity/port carry over from the placement.
    pub fn sshfs_mount_argv(&self, remote_path: &str, mountpoint: &str) -> Vec<String> {
        let mut v = vec![
            "sshfs".to_string(),
            format!("{}:{remote_path}", self.host),
            mountpoint.to_string(),
        ];
        if self.port != 22 {
            v.push("-p".into());
            v.push(self.port.to_string());
        }
        v.extend([
            "-o".into(),
            "reconnect".into(),
            "-o".into(),
            "ServerAliveInterval=15".into(),
            "-o".into(),
            "ServerAliveCountMax=3".into(),
        ]);
        if let Some(id) = &self.identity {
            v.push("-o".into());
            v.push(format!("IdentityFile={id}"));
        }
        v
    }

    /// argv to unmount a previously sshfs-mounted `mountpoint`.
    pub fn sshfs_unmount_argv(mountpoint: &str) -> Vec<String> {
        vec!["fusermount3".into(), "-u".into(), mountpoint.to_string()]
    }
}

/// A Kubernetes placement: processes run inside a pod via `kubectl exec`. The
/// pod itself is the isolation boundary, so the sandbox backend is typically
/// `none`. Carries the lifecycle inputs (`pod_template`/`image`) so it can both
/// build the exec argv and spawn/tear down the pod ([`Placement::ensure`]).
#[derive(Debug, Clone)]
pub struct K8sPlacement {
    pub kubectl: String,
    pub context: Option<String>,
    pub namespace: Option<String>,
    /// Resolved pod name (or `name=…`/`pod/…` selector accepted by kubectl).
    pub pod: String,
    /// Optional container within the pod (`-c`).
    pub container: Option<String>,
    /// A manifest applied to spawn the pod (`kubectl apply -f`); idempotent.
    pub pod_template: Option<String>,
    /// Image for a template-less pod (`kubectl run <pod> --image=…`).
    pub image: Option<String>,
}

impl K8sPlacement {
    fn base(&self) -> Vec<String> {
        let mut v = vec![self.kubectl.clone()];
        if let Some(c) = &self.context {
            v.push("--context".into());
            v.push(c.clone());
        }
        if let Some(n) = &self.namespace {
            v.push("--namespace".into());
            v.push(n.clone());
        }
        v
    }

    /// argv that spawns/ensures the pod, or `None` when the pod is assumed to
    /// pre-exist (no `pod_template`/`image`). `apply` is idempotent; `run`
    /// tolerates an existing pod via `--field-manager` semantics + the caller's
    /// best-effort handling.
    pub fn ensure_argv(&self) -> Option<Vec<String>> {
        let mut v = self.base();
        if let Some(tpl) = &self.pod_template {
            v.extend(["apply".into(), "-f".into(), tpl.clone()]);
            Some(v)
        } else if let Some(img) = &self.image {
            v.extend([
                "run".into(),
                self.pod.clone(),
                format!("--image={img}"),
                "--restart=Never".into(),
                "--command".into(),
                "--".into(),
                "sleep".into(),
                "infinity".into(),
            ]);
            Some(v)
        } else {
            None
        }
    }

    /// argv that blocks until the pod reports `Ready`.
    pub fn wait_argv(&self) -> Vec<String> {
        let mut v = self.base();
        v.extend([
            "wait".into(),
            "--for=condition=Ready".into(),
            format!("pod/{}", self.pod),
            "--timeout=120s".into(),
        ]);
        v
    }

    /// argv that tears the pod down (deletes the manifest, or the named pod).
    pub fn teardown_argv(&self) -> Vec<String> {
        let mut v = self.base();
        match &self.pod_template {
            Some(tpl) => v.extend([
                "delete".into(),
                "-f".into(),
                tpl.clone(),
                "--ignore-not-found".into(),
            ]),
            None => v.extend([
                "delete".into(),
                "pod".into(),
                self.pod.clone(),
                "--ignore-not-found".into(),
            ]),
        }
        v
    }

    /// argv that forwards a `local:remote` (or bare `port`) spec from the pod to
    /// localhost — the "remote access" feature for k8s envs.
    pub fn port_forward_argv(&self, spec: &str) -> Vec<String> {
        let mut v = self.base();
        v.extend([
            "port-forward".into(),
            format!("pod/{}", self.pod),
            spec.to_string(),
        ]);
        v
    }

    fn exec_wrap(&self, argv: &[String], interactive: bool) -> Vec<String> {
        let mut v = self.base();
        v.push("exec".into());
        v.push(if interactive {
            "-it".into()
        } else {
            "-i".into()
        });
        v.push(self.pod.clone());
        if let Some(c) = &self.container {
            v.push("-c".into());
            v.push(c.clone());
        }
        v.push("--".into());
        v.push("/bin/sh".into());
        v.push("-lc".into());
        v.push(util::sh_join(argv));
        v
    }
}

/// A provider-managed placement (Daytona, Codespaces, …). The provider resolves
/// a sandbox to an exec prefix — either a CLI (`daytona ssh <id> --`) or an ssh
/// argv. The interactive/control distinction lets a provider use a PTY-capable
/// command for the pane and a batch command for control-plane probes.
#[derive(Debug, Clone)]
pub struct ProviderPlacement {
    /// Provider id, e.g. `"daytona"` — for status display and teardown routing.
    pub provider: String,
    /// Opaque sandbox/environment id (for status + lifecycle).
    pub id: String,
    /// Argv prefix for the interactive pane (a PTY-capable exec command).
    pub interactive_prefix: Vec<String>,
    /// Argv prefix for non-interactive control-plane calls.
    pub control_prefix: Vec<String>,
    /// Full argv to create/start the sandbox (empty ⇒ assumed pre-created).
    pub up_command: Vec<String>,
    /// Full argv to destroy/stop the sandbox (empty ⇒ no teardown).
    pub down_command: Vec<String>,
}

impl ProviderPlacement {
    fn wrap(prefix: &[String], argv: &[String]) -> Vec<String> {
        let mut v = prefix.to_vec();
        v.push("/bin/sh".into());
        v.push("-lc".into());
        v.push(util::sh_join(argv));
        v
    }
}

/// The resolved runtime placement for a worktree's processes.
#[derive(Debug, Clone)]
pub enum Placement {
    Local,
    Ssh(SshPlacement),
    K8s(K8sPlacement),
    Provider(ProviderPlacement),
}

impl Placement {
    /// Local placement runs argv directly with no wrapping.
    pub fn is_local(&self) -> bool {
        matches!(self, Placement::Local)
    }

    /// A short, human-readable label for status surfaces (sidebar/panel).
    pub fn label(&self) -> String {
        match self {
            Placement::Local => "local".into(),
            Placement::Ssh(s) => match s.kind {
                TransportKind::Mosh => format!("mosh:{}", s.host),
                TransportKind::Ssh => format!("ssh:{}", s.host),
            },
            Placement::K8s(k) => match &k.namespace {
                Some(ns) => format!("k8s:{ns}/{}", k.pod),
                None => format!("k8s:{}", k.pod),
            },
            Placement::Provider(p) => format!("{}:{}", p.provider, p.id),
        }
    }

    /// Wrap a backend argv for the interactive pane.
    pub fn interactive_argv(&self, backend_argv: &[String]) -> Vec<String> {
        match self {
            Placement::Local => backend_argv.to_vec(),
            Placement::Ssh(s) => s.interactive_wrap(backend_argv),
            Placement::K8s(k) => k.exec_wrap(backend_argv, true),
            Placement::Provider(p) => ProviderPlacement::wrap(&p.interactive_prefix, backend_argv),
        }
    }

    /// Wrap an argv for a non-interactive control-plane call (probes, container
    /// lifecycle). Local returns it unchanged.
    pub fn control_argv(&self, argv: &[String]) -> Vec<String> {
        match self {
            Placement::Local => argv.to_vec(),
            Placement::Ssh(s) => s.control_wrap(argv),
            Placement::K8s(k) => k.exec_wrap(argv, false),
            Placement::Provider(p) => ProviderPlacement::wrap(&p.control_prefix, argv),
        }
    }

    /// Is the named binary present in this placement? (Local: PATH; remote: a
    /// `command -v` probe through the placement's control primitive.)
    pub fn has_binary(&self, bin: &str) -> bool {
        match self {
            Placement::Local => util::have(bin),
            _ => {
                let probe = vec![format!("command -v {bin} >/dev/null 2>&1")];
                let argv = self.control_argv(&probe);
                std::process::Command::new(&argv[0])
                    .args(&argv[1..])
                    .output()
                    .map(|o| o.status.success())
                    .unwrap_or(false)
            }
        }
    }

    /// Bring the placement's environment up: for `k8s`, apply/run the pod and
    /// wait for it to be `Ready`. Local/ssh/provider are no-ops here (an ssh
    /// host pre-exists; a static-id provider sandbox is pre-created — the async
    /// provider create/discover lifecycle lives in `superzej-svc`). Best-effort
    /// errors are returned so the caller can surface them without blocking.
    pub fn ensure(&self) -> Result<(), String> {
        match self {
            Placement::K8s(k) => {
                if let Some(argv) = k.ensure_argv() {
                    // `run` errors if the pod already exists — tolerate that, the
                    // subsequent wait confirms readiness either way.
                    let _ = run_argv(&argv);
                }
                run_argv(&k.wait_argv())
            }
            Placement::Provider(p) if !p.up_command.is_empty() => run_argv(&p.up_command),
            _ => Ok(()),
        }
    }

    /// Tear the placement's environment down (k8s: delete the pod/manifest;
    /// provider: run its `down_command`).
    pub fn teardown(&self) -> Result<(), String> {
        match self {
            Placement::K8s(k) => run_argv(&k.teardown_argv()),
            Placement::Provider(p) if !p.down_command.is_empty() => run_argv(&p.down_command),
            _ => Ok(()),
        }
    }

    /// argv that forwards a port from the environment to localhost, when the
    /// placement supports it (k8s `port-forward`). `None` otherwise.
    pub fn port_forward_argv(&self, spec: &str) -> Option<Vec<String>> {
        match self {
            Placement::K8s(k) => Some(k.port_forward_argv(spec)),
            _ => None,
        }
    }
}

/// Run an argv to completion, mapping a non-zero exit (or spawn failure) to an
/// `Err` with the captured stderr — the shared runner for placement lifecycle.
fn run_argv(argv: &[String]) -> Result<(), String> {
    let Some((cmd, rest)) = argv.split_first() else {
        return Err("empty command".into());
    };
    match std::process::Command::new(cmd).args(rest).output() {
        Ok(o) if o.status.success() => Ok(()),
        Ok(o) => Err(format!(
            "`{}` failed: {}",
            argv.join(" "),
            String::from_utf8_lossy(&o.stderr).trim()
        )),
        Err(e) => Err(format!("`{}` could not run: {e}", argv.join(" "))),
    }
}

/// Reconnect policy for a dropped remote transport (ssh/mosh/k8s-exec pane).
/// Pure — decides whether a re-spawn is warranted and the backoff before each
/// attempt — so the host can wrap a remote pane in a reconnect loop without
/// hard-coding the cadence. mosh self-heals roaming; this covers the ssh/exec
/// case where the channel drops and the pane process exits 255.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReconnectPolicy {
    pub max_attempts: u32,
    pub base_delay_ms: u64,
    pub max_delay_ms: u64,
}

impl Default for ReconnectPolicy {
    fn default() -> Self {
        ReconnectPolicy {
            max_attempts: 5,
            base_delay_ms: 500,
            max_delay_ms: 10_000,
        }
    }
}

impl ReconnectPolicy {
    /// Exponential backoff before `attempt` (1-based), capped at `max_delay_ms`;
    /// `None` once attempts are exhausted (`attempt > max_attempts`) or zero.
    pub fn backoff(&self, attempt: u32) -> Option<std::time::Duration> {
        if attempt == 0 || attempt > self.max_attempts {
            return None;
        }
        let shift = (attempt - 1).min(20);
        let ms = self
            .base_delay_ms
            .saturating_mul(1u64 << shift)
            .min(self.max_delay_ms);
        Some(std::time::Duration::from_millis(ms))
    }

    /// Whether a remote pane that exited with `exit_code` on its `attempt`-th
    /// run should reconnect. ssh/mosh report a connection drop as 255; a clean
    /// or application exit (anything else) is terminal — the user quit the shell.
    pub fn should_reconnect(&self, exit_code: i32, attempt: u32) -> bool {
        attempt <= self.max_attempts && exit_code == 255
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_is_passthrough() {
        let p = Placement::Local;
        let argv = vec!["podman".to_string(), "exec".into(), "x".into()];
        assert_eq!(p.interactive_argv(&argv), argv);
        assert_eq!(p.control_argv(&argv), argv);
        assert!(p.is_local());
        assert_eq!(p.label(), "local");
    }

    #[test]
    fn ssh_plain_matches_legacy_shape() {
        let p = Placement::Ssh(SshPlacement::plain(
            "dev@box".into(),
            22,
            true,
            TransportKind::Ssh,
        ));
        let argv = vec!["podman".to_string(), "exec".into(), "c".into()];
        let out = p.interactive_argv(&argv);
        // ssh ... -A ... -t dev@box -- /bin/sh -lc '<joined>'
        assert_eq!(out[0], "ssh");
        assert!(out.contains(&"-A".to_string()));
        assert!(out.contains(&"-t".to_string()));
        assert!(out.contains(&"dev@box".to_string()));
        assert_eq!(out[out.len() - 3], "/bin/sh");
        assert_eq!(out[out.len() - 2], "-lc");
        assert_eq!(out[out.len() - 1], util::sh_join(&argv));
        // Plain target adds no -F/-J/-i.
        assert!(!out.contains(&"-F".to_string()));
        assert!(!out.contains(&"-J".to_string()));
    }

    #[test]
    fn ssh_mosh_wraps_with_ssh_subcommand() {
        let p = Placement::Ssh(SshPlacement::plain(
            "h".into(),
            2222,
            false,
            TransportKind::Mosh,
        ));
        let out = p.interactive_argv(&["echo".into(), "hi".into()]);
        assert_eq!(out[0], "mosh");
        assert!(out[1].starts_with("--ssh="));
        // Non-default port is carried into the inner ssh.
        assert!(out[1].contains("-p"));
        assert!(out[1].contains("2222"));
        assert_eq!(out[2], "h");
    }

    #[test]
    fn ssh_knobs_appended_only_when_set() {
        let mut s = SshPlacement::plain("h".into(), 22, false, TransportKind::Ssh);
        s.ssh_config = Some("/cfg".into());
        s.jump_host = Some("bastion".into());
        s.identity = Some("/id".into());
        s.extra_args = vec!["-o".into(), "StrictHostKeyChecking=no".into()];
        let base = s.ssh_base(true);
        assert!(base.windows(2).any(|w| w == ["-F", "/cfg"]));
        assert!(base.windows(2).any(|w| w == ["-J", "bastion"]));
        assert!(base.windows(2).any(|w| w == ["-i", "/id"]));
        assert!(
            base.windows(2)
                .any(|w| w == ["-o", "StrictHostKeyChecking=no"])
        );
    }

    #[test]
    fn k8s_exec_argv() {
        let p = Placement::K8s(K8sPlacement {
            kubectl: "kubectl".into(),
            context: Some("company-prod".into()),
            namespace: Some("dev-blake".into()),
            pod: "sz-pod".into(),
            container: Some("dev".into()),
            pod_template: None,
            image: None,
        });
        let out = p.interactive_argv(&["echo".into(), "x".into()]);
        assert_eq!(out[0], "kubectl");
        assert!(out.windows(2).any(|w| w == ["--context", "company-prod"]));
        assert!(out.windows(2).any(|w| w == ["--namespace", "dev-blake"]));
        assert!(out.contains(&"exec".to_string()));
        assert!(out.contains(&"-it".to_string()));
        assert!(out.windows(2).any(|w| w == ["-c", "dev"]));
        assert_eq!(out[out.len() - 3], "/bin/sh");
        // control uses -i (no tty)
        let ctl = p.control_argv(&["true".into()]);
        assert!(ctl.contains(&"-i".to_string()));
        assert!(!ctl.contains(&"-it".to_string()));
    }

    #[test]
    fn control_argv_and_labels_across_variants() {
        // Local control is a passthrough and reports a missing binary as absent.
        let local = Placement::Local;
        assert_eq!(
            local.control_argv(&["true".into()]),
            vec!["true".to_string()]
        );
        assert!(!local.has_binary("definitely-not-a-real-binary-zzz"));

        // SSH control wraps with a batch ssh; label reflects the transport.
        let ssh = Placement::Ssh(SshPlacement::plain(
            "h".into(),
            22,
            false,
            TransportKind::Ssh,
        ));
        let ctl = ssh.control_argv(&["echo".into(), "x".into()]);
        assert_eq!(ctl[0], "ssh");
        assert_eq!(ctl[ctl.len() - 2], "--");
        assert_eq!(ssh.label(), "ssh:h");
        assert_eq!(
            Placement::Ssh(SshPlacement::plain(
                "h".into(),
                22,
                false,
                TransportKind::Mosh
            ))
            .label(),
            "mosh:h"
        );

        // K8s label without a namespace omits the `ns/` prefix.
        let k8s = Placement::K8s(K8sPlacement {
            kubectl: "kubectl".into(),
            context: None,
            namespace: None,
            pod: "p".into(),
            container: None,
            pod_template: None,
            image: None,
        });
        assert_eq!(k8s.label(), "k8s:p");

        // Provider control uses the control prefix.
        let prov = Placement::Provider(ProviderPlacement {
            provider: "daytona".into(),
            id: "i".into(),
            interactive_prefix: vec!["a".into()],
            control_prefix: vec!["b".into()],
            up_command: Vec::new(),
            down_command: Vec::new(),
        });
        assert_eq!(prov.control_argv(&["x".into()])[0], "b");
        // No up/down command ⇒ ensure/teardown are no-ops.
        assert!(prov.ensure().is_ok());
        assert!(prov.teardown().is_ok());
    }

    #[test]
    fn k8s_lifecycle_argv() {
        // Template-driven: apply/delete the manifest; wait + port-forward target the pod.
        let tmpl = K8sPlacement {
            kubectl: "kubectl".into(),
            context: Some("ctx".into()),
            namespace: Some("ns".into()),
            pod: "sz".into(),
            container: None,
            pod_template: Some("/tmp/pod.yaml".into()),
            image: None,
        };
        let ensure = tmpl.ensure_argv().unwrap();
        assert!(ensure.windows(2).any(|w| w == ["apply", "-f"]));
        assert!(ensure.contains(&"/tmp/pod.yaml".to_string()));
        assert!(tmpl.wait_argv().contains(&"pod/sz".to_string()));
        assert!(
            tmpl.teardown_argv()
                .windows(2)
                .any(|w| w == ["-f", "/tmp/pod.yaml"])
        );
        assert!(
            tmpl.teardown_argv()
                .contains(&"--ignore-not-found".to_string())
        );
        let pf = tmpl.port_forward_argv("8080:80");
        assert!(pf.contains(&"port-forward".to_string()));
        assert!(pf.contains(&"8080:80".to_string()));

        // Image-driven: `kubectl run` spawns; delete targets the named pod.
        let img = K8sPlacement {
            kubectl: "kubectl".into(),
            context: None,
            namespace: None,
            pod: "sz".into(),
            container: None,
            pod_template: None,
            image: Some("debian:stable".into()),
        };
        let ensure = img.ensure_argv().unwrap();
        assert_eq!(ensure[1], "run");
        assert!(ensure.contains(&"--image=debian:stable".to_string()));
        assert!(img.teardown_argv().windows(2).any(|w| w == ["pod", "sz"]));

        // No template/image ⇒ pod assumed pre-existing (nothing to ensure).
        let bare = K8sPlacement {
            kubectl: "kubectl".into(),
            context: None,
            namespace: None,
            pod: "sz".into(),
            container: None,
            pod_template: None,
            image: None,
        };
        assert!(bare.ensure_argv().is_none());

        // ensure()/teardown() are no-ops (Ok) for non-k8s placements.
        assert!(Placement::Local.ensure().is_ok());
        assert!(Placement::Local.teardown().is_ok());
        assert!(Placement::Local.port_forward_argv("80").is_none());
    }

    #[test]
    fn sshfs_mount_and_unmount_argv() {
        let mut s = SshPlacement::plain("u@box".into(), 2222, true, TransportKind::Ssh);
        s.identity = Some("/id".into());
        let m = s.sshfs_mount_argv("/srv/wt", "/local/mnt");
        assert_eq!(m[0], "sshfs");
        assert_eq!(m[1], "u@box:/srv/wt");
        assert_eq!(m[2], "/local/mnt");
        assert!(m.windows(2).any(|w| w == ["-p", "2222"]));
        assert!(m.windows(2).any(|w| w == ["-o", "reconnect"]));
        assert!(m.iter().any(|a| a == "IdentityFile=/id"));
        let u = SshPlacement::sshfs_unmount_argv("/local/mnt");
        assert_eq!(u, vec!["fusermount3", "-u", "/local/mnt"]);
    }

    #[test]
    fn reconnect_policy_backoff_and_decision() {
        let p = ReconnectPolicy::default();
        // Exponential, capped, then exhausted.
        assert_eq!(p.backoff(1).unwrap().as_millis(), 500);
        assert_eq!(p.backoff(2).unwrap().as_millis(), 1000);
        assert_eq!(p.backoff(5).unwrap().as_millis(), 8000);
        assert_eq!(p.backoff(0), None);
        assert_eq!(p.backoff(6), None);
        // Only a connection drop (255) reconnects, and only within budget.
        assert!(p.should_reconnect(255, 1));
        assert!(p.should_reconnect(255, 5));
        assert!(!p.should_reconnect(255, 6));
        assert!(!p.should_reconnect(0, 1));
        assert!(!p.should_reconnect(130, 1));
        // A cap that bites.
        let capped = ReconnectPolicy {
            max_attempts: 10,
            base_delay_ms: 1000,
            max_delay_ms: 3000,
        };
        assert_eq!(capped.backoff(8).unwrap().as_millis(), 3000);
    }

    #[test]
    fn provider_wraps_with_prefix() {
        let p = Placement::Provider(ProviderPlacement {
            provider: "daytona".into(),
            id: "abc123".into(),
            interactive_prefix: vec!["daytona".into(), "ssh".into(), "abc123".into(), "--".into()],
            control_prefix: vec!["daytona".into(), "ssh".into(), "abc123".into(), "--".into()],
            up_command: Vec::new(),
            down_command: Vec::new(),
        });
        let out = p.interactive_argv(&["ls".into()]);
        assert_eq!(&out[..4], &["daytona", "ssh", "abc123", "--"]);
        assert_eq!(out[4], "/bin/sh");
        assert_eq!(p.label(), "daytona:abc123");
    }
}
