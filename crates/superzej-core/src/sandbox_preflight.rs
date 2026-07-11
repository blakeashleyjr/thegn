//! Preflight exec probe for OCI sandboxes — split out of the (ratchet-capped)
//! `sandbox.rs`.
//!
//! Podman/Docker are a two-step model: [`sandbox::ensure`](crate::sandbox::ensure)
//! creates and verifies a keep-alive container, then the interactive pane runs
//! `podman exec -it … /bin/sh -lc <shell>` into it. `ensure` proves the container
//! reached `RUNNING`, but it does **not** prove the *exec* path works — on some
//! hosts the container runs yet `exec` fails (e.g. a broken `--userns keep-id`
//! crun combination, an image without `/bin/sh`, a workdir the mount didn't
//! provide). When that happens the pane exits inside the fast-crash window and
//! silently disappears, the real error lost with it.
//!
//! [`preflight_exec`] closes that gap: after `ensure` succeeds we run the *same*
//! exec path with a trivial `true`, capturing stderr. The caller
//! (the host's `agent::prepare_sandbox_env`) turns a failure into a legible error — a hard
//! block for an explicit backend, or a visible fall-through to the next chain
//! entry (bwrap) for `auto`. Bwrap and `none` are no-ops (single-command
//! backends with nothing to exec into).

use crate::config::FileAccess;
use crate::sandbox::{PROBE_TIMEOUT, SandboxSpec, oci_prefix};

/// The exec argv the preflight probe runs. It mirrors the OCI arm of
/// `backend_enter_argv` exactly — same `oci_prefix` (so it targets the right
/// local/remote daemon), same `--workdir <worktree>` gated on file access, same
/// `<name> /bin/sh -lc <body>` shape — with two deliberate differences: the body
/// is `true` (cheap, always succeeds *if the container can exec at all*) and
/// there is no `-it` (a capture probe has no controlling TTY). Wrapped through
/// the placement so a remote OCI target is probed on its own host.
pub(crate) fn preflight_exec_argv(spec: &SandboxSpec) -> Vec<String> {
    let mut v = oci_prefix(spec);
    v.push("exec".into());
    // Match the pane: only pass --workdir when the worktree is actually mounted.
    if spec.file_access != FileAccess::None {
        v.push("--workdir".into());
        v.push(spec.worktree.to_string_lossy().into_owned());
    }
    v.push(spec.name.clone());
    v.extend(["/bin/sh".into(), "-lc".into(), "true".into()]);
    spec.placement.control_argv(&v)
}

/// Verify the pane's exec path works before we spawn the (doomed) pane.
///
/// `Ok(())` when a trivial `exec` into the container succeeds — the real pane
/// will too. `Err(reason)` carries the container/runtime stderr (trimmed) on
/// failure, or a synthetic message on timeout. No-op (`Ok`) for non-OCI backends,
/// which have no container to exec into.
///
/// Subprocess seam (`cov_ignore`, like the rest of the sandbox runtime); the pure
/// argv builder above is unit-tested.
pub fn preflight_exec(spec: &SandboxSpec) -> Result<(), String> {
    if !spec.backend.is_oci() {
        return Ok(());
    }
    let argv = preflight_exec_argv(spec);
    match output_stderr_with_timeout(&argv, PROBE_TIMEOUT) {
        Some((true, _)) => Ok(()),
        Some((false, stderr)) => {
            let msg = stderr.trim();
            Err(if msg.is_empty() {
                format!("exec probe failed for container '{}'", spec.name)
            } else {
                // Cap the surfaced text; a runtime can be verbose.
                msg.chars().take(400).collect()
            })
        }
        None => Err(format!(
            "exec probe timed out after {}s for container '{}'",
            PROBE_TIMEOUT.as_secs(),
            spec.name
        )),
    }
}

/// Like `sandbox::output_with_timeout` but captures **stderr** (where a container
/// runtime writes its failure text) instead of stdout. Returns `(success, stderr)`
/// or `None` on spawn failure or timeout (child killed + reaped). The probed
/// command is `true`, so its output is tiny — reading after exit can't deadlock.
fn output_stderr_with_timeout(
    argv: &[String],
    timeout: std::time::Duration,
) -> Option<(bool, String)> {
    use std::process::{Command, Stdio};
    use std::time::Instant;
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
                let stderr = child
                    .stderr
                    .take()
                    .and_then(|mut r| {
                        use std::io::Read;
                        let mut s = String::new();
                        r.read_to_string(&mut s).ok().map(|_| s)
                    })
                    .unwrap_or_default();
                return Some((status.success(), stderr));
            }
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(25)),
            Err(_) => return None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Network;
    use crate::placement::Placement;
    use crate::sandbox::{Backend, Mount, SandboxLimits};
    use std::path::PathBuf;

    /// Minimal local spec builder (sandbox.rs's own `spec()` helper is private to
    /// its test module, and sandbox.rs is at the file-size hard cap so we can't
    /// export a shared one — replicate the few fields we exercise here).
    fn base_spec(backend: Backend) -> SandboxSpec {
        SandboxSpec {
            backend,
            placement: Placement::Local,
            image: Some("img:latest".into()),
            worktree: PathBuf::from("/wt/feat"),
            mounts: vec![Mount {
                host: "/wt/feat".into(),
                dest: "/wt/feat".into(),
                ro: false,
                cache: false,
            }],
            env: Vec::new(),
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
            ports: Vec::new(),
            gpu: None,
            limits: SandboxLimits::default(),
            volumes: vec![],
            compose: None,
            build: None,
            init_script: None,
            file_access: FileAccess::Worktree,
            devenv: false,
            devenv_path: None,
            name: "superzej-repo-feat".into(),
            vpn: None,
            oci_host: None,
        }
    }

    #[test]
    fn argv_matches_pane_exec_path() {
        let mut s = base_spec(Backend::Podman);
        s.worktree = std::path::PathBuf::from("/wt/feat");
        s.name = "superzej-repo-feat".into();
        let argv = preflight_exec_argv(&s);
        // Same shape as the pane exec, but `true` and no `-it`.
        assert_eq!(argv[0], "podman");
        assert!(argv.contains(&"exec".to_string()));
        assert!(
            !argv.contains(&"-it".to_string()),
            "probe must not request a TTY"
        );
        assert!(argv.contains(&"--workdir".to_string()));
        assert!(argv.contains(&"/wt/feat".to_string()));
        assert!(argv.contains(&"superzej-repo-feat".to_string()));
        // `true` is the trailing command.
        assert_eq!(argv.last().map(String::as_str), Some("true"));
        let sh = argv.iter().position(|a| a == "/bin/sh").unwrap();
        assert_eq!(argv[sh + 1], "-lc");
        assert_eq!(argv[sh + 2], "true");
    }

    #[test]
    fn file_access_none_drops_workdir() {
        let mut s = base_spec(Backend::Podman);
        s.file_access = FileAccess::None;
        let argv = preflight_exec_argv(&s);
        assert!(
            !argv.contains(&"--workdir".to_string()),
            "no worktree mount ⇒ no --workdir"
        );
        assert_eq!(argv.last().map(String::as_str), Some("true"));
    }

    #[test]
    fn targets_remote_daemon_connection() {
        let mut s = base_spec(Backend::Podman);
        s.oci_host = Some("ci-box".into());
        let argv = preflight_exec_argv(&s);
        // oci_prefix injects the connection flag right after the binary.
        assert!(argv.contains(&"--connection".to_string()));
        assert!(argv.contains(&"ci-box".to_string()));
    }

    #[test]
    fn preflight_noop_for_non_oci_backends() {
        // No subprocess is spawned for bwrap/none — safe in the gated suite.
        assert_eq!(preflight_exec(&base_spec(Backend::Bwrap)), Ok(()));
        assert_eq!(preflight_exec(&base_spec(Backend::None)), Ok(()));
    }
}
