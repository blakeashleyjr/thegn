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

pub(crate) type PreflightRunner = dyn Fn(&[String]) -> Option<(bool, String)>;

/// The exec argv the preflight probe runs. It mirrors the OCI arm of
/// `backend_enter_argv` exactly — same `oci_prefix` (so it targets the right
/// local/remote daemon), same `--workdir <worktree>` gated on file access, same
/// `<name> /bin/sh -lc <body>` shape — with two deliberate differences: the body
/// is `true` (cheap, always succeeds *if the container can exec at all*) and
/// there is no `-it` (a capture probe has no controlling TTY). Wrapped through
/// the placement so a remote OCI target is probed on its own host.
pub(crate) fn preflight_exec_argv(spec: &SandboxSpec) -> Result<Vec<String>, String> {
    // Seam: the only fs touch. `mount_sentinels` is pure over `host_exists`, and
    // returns nothing when it cannot prove a failure — in which case the body is
    // the literal `true` this probe used before.
    let sentinels =
        crate::sandbox_mountcheck::mount_sentinels(spec, &|p| std::path::Path::new(p).exists());
    preflight_exec_argv_with(
        spec,
        &crate::sandbox_mountcheck::preflight_probe_body(&sentinels),
    )
}

/// [`preflight_exec_argv`] with the probe body chosen by the caller — the pure
/// half, so the argv shape is unit-tested without touching the filesystem.
pub(crate) fn preflight_exec_argv_with(
    spec: &SandboxSpec,
    body: &str,
) -> Result<Vec<String>, String> {
    if let Some(compose) = spec
        .compose_spec()
        .filter(|compose| spec.backend.is_oci() && compose.has_service())
    {
        let workdir = (spec.file_access != FileAccess::None)
            .then(|| spec.worktree.to_string_lossy().into_owned());
        let target = crate::sandbox_compose::ComposeTarget::for_spec(spec)
            .map_err(|error| error.to_string())?;
        let argv = crate::sandbox_compose::exec_argv(
            &target,
            &spec.name,
            &compose,
            workdir.as_deref(),
            body,
            false,
        )
        .ok_or_else(|| "Compose service is missing".to_string())?;
        return Ok(spec.placement.control_argv(&argv));
    }
    let mut v = oci_prefix(spec);
    v.push("exec".into());
    // Match the pane: only pass --workdir when the worktree is actually mounted.
    if spec.file_access != FileAccess::None {
        v.push("--workdir".into());
        v.push(spec.worktree.to_string_lossy().into_owned());
    }
    v.push(spec.name.clone());
    v.extend(["/bin/sh".into(), "-lc".into(), body.to_string()]);
    Ok(spec.placement.control_argv(&v))
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
    preflight_exec_with(spec, &|argv| {
        output_stderr_with_timeout(argv, PROBE_TIMEOUT)
    })
}

/// [`preflight_exec`] with an injected executor, allowing route and failure
/// behavior to be tested without a live container engine.
pub(crate) fn preflight_exec_with(spec: &SandboxSpec, run: &PreflightRunner) -> Result<(), String> {
    if !spec.backend.is_oci() {
        return Ok(());
    }
    let argv = preflight_exec_argv(spec)?;
    match run(&argv) {
        Some((true, _)) => Ok(()),
        Some((false, stderr)) => {
            // A verified mount failure is its own class, with its own remedy.
            if let Some(missing) = crate::sandbox_mountcheck::parse_missing_sentinel(&stderr) {
                // MUST remove the container. Its binds were fixed at create, and
                // `sandbox::container_status` compares our own mount request to
                // the runtime's echo of it — so it will call this container
                // healthy forever. Leave it and the user widens the VM share,
                // relaunches, and lands in the SAME empty directory.
                let probe = crate::sandbox_mountcheck::MountProbe {
                    backend: spec.backend,
                    os: crate::sandbox_backend::host_os(),
                    file_access: spec.file_access,
                    // Canonicalized: on macOS `/tmp` and `/var` are symlinks into
                    // `/private`, which is what the VM actually shares, so the
                    // raw path yields the wrong share root in the remedy.
                    worktree: &canonical_worktree(spec),
                    missing,
                };
                let failure = crate::sandbox_mountcheck::mount_failure(&probe, &|b| {
                    crate::util::which_path(b).is_some()
                });
                if !spec
                    .compose_spec()
                    .is_some_and(|compose| compose.has_service())
                {
                    crate::sandbox::remove_container(spec);
                }
                return Err(failure.one_line());
            }
            let msg = stderr.trim();
            if spec
                .compose_spec()
                .is_some_and(|compose| compose.has_service())
                && let Some(reason) = compose_exec_failure(msg)
            {
                return Err(reason);
            }
            Err(if msg.is_empty() {
                format!("exec probe failed for container '{}'", spec.name)
            } else {
                // Cap the surfaced text; a runtime can be verbose.
                format!(
                    "exec probe failed: {}",
                    msg.chars().take(400).collect::<String>()
                )
            })
        }
        None => Err(format!(
            "exec probe timed out after {}s for container '{}'",
            PROBE_TIMEOUT.as_secs(),
            spec.name
        )),
    }
}

fn compose_exec_failure(stderr: &str) -> Option<String> {
    let lower = stderr.to_ascii_lowercase();
    if lower.contains("no such service") || lower.contains("no service found") {
        Some("Compose preflight failed: service was not found".into())
    } else if lower.contains("is not running")
        || lower.contains("no container found for")
        || lower.contains("service has no container")
    {
        Some("Compose preflight failed: service has no running container".into())
    } else {
        None
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
    crate::sandbox::proc::run_bounded(
        argv,
        timeout,
        crate::sandbox::proc::Capture {
            stdout: None,
            stderr: Some(256 * 1024),
        },
    )
    .into_stderr()
}

/// The worktree path with symlinks resolved, for the remedy's share-root guess.
/// Falls back to the raw path when it cannot be resolved — a wrong-but-present
/// root is better than no remedy.
pub(crate) fn canonical_worktree(spec: &SandboxSpec) -> String {
    std::fs::canonicalize(&spec.worktree)
        .unwrap_or_else(|_| spec.worktree.clone())
        .to_string_lossy()
        .into_owned()
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
            name: "thegn-repo-feat".into(),
            vpn: None,
            oci_host: None,
            oci_runtime: None,
            daemon_persistent: false,
            seal_home: None,
        }
    }

    #[test]
    fn argv_matches_pane_exec_path() {
        let mut s = base_spec(Backend::Podman);
        s.worktree = std::path::PathBuf::from("/wt/feat");
        s.name = "thegn-repo-feat".into();
        let argv = preflight_exec_argv(&s).unwrap();
        // Same shape as the pane exec, but `true` and no `-it`.
        assert_eq!(argv[0], "podman");
        assert!(argv.contains(&"exec".to_string()));
        assert!(
            !argv.contains(&"-it".to_string()),
            "probe must not request a TTY"
        );
        assert!(argv.contains(&"--workdir".to_string()));
        assert!(argv.contains(&"/wt/feat".to_string()));
        assert!(argv.contains(&"thegn-repo-feat".to_string()));
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
        let argv = preflight_exec_argv(&s).unwrap();
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
        let argv = preflight_exec_argv(&s).unwrap();
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

    #[test]
    fn compose_preflight_routes_to_service_and_non_oci_never_routes_to_compose() {
        let compose = crate::sandbox_compose::ComposeSpec {
            files: vec!["compose.yml".into()],
            service: Some("worker".into()),
            run_services: Vec::new(),
        };
        let mut spec = base_spec(Backend::Docker);
        spec.compose = Some(compose.encode());
        let routed = preflight_exec_argv_with(&spec, "true").unwrap();
        let workdir = spec.worktree.to_string_lossy().into_owned();
        let pane_path = crate::sandbox_compose::exec_argv(
            &crate::sandbox_compose::ComposeTarget::for_spec(&spec).unwrap(),
            &spec.name,
            &compose,
            Some(&workdir),
            "true",
            false,
        )
        .unwrap();
        assert_eq!(routed, spec.placement.control_argv(&pane_path));
        let exec = routed.iter().position(|arg| arg == "exec").unwrap();
        assert!(routed[exec + 1..].iter().any(|arg| arg == "worker"));
        assert!(!routed.contains(&"-it".into()));

        spec.backend = Backend::Bwrap;
        let bwrap = preflight_exec_argv_with(&spec, "true").unwrap();
        assert_eq!(bwrap.first().map(String::as_str), Some("bwrap"));
        assert!(!bwrap.iter().any(|arg| arg == "compose"));
    }

    #[test]
    fn compose_preflight_runner_targets_service_not_container_name() {
        let compose = crate::sandbox_compose::ComposeSpec {
            files: vec!["compose.yml".into()],
            service: Some("app".into()),
            run_services: Vec::new(),
        };
        let mut spec = base_spec(Backend::Docker);
        spec.name = "thegn-x".into();
        spec.compose = Some(compose.encode());
        let observed = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let runner_observed = std::sync::Arc::clone(&observed);
        let result = preflight_exec_with(&spec, &move |argv| {
            *runner_observed.lock().unwrap() = argv.to_vec();
            Some((true, String::new()))
        });
        assert_eq!(result, Ok(()));
        let observed = observed.lock().unwrap();
        let exec = observed.iter().position(|arg| arg == "exec").unwrap();
        assert_eq!(observed[exec - 2].as_str(), "-p");
        assert_eq!(observed[exec - 1].as_str(), "thegn-x");
        let after_exec = &observed[exec + 1..];
        assert!(!after_exec.iter().any(|arg| arg == "thegn-x"));
        let service_index = if after_exec.first().is_some_and(|arg| arg == "--workdir") {
            2
        } else {
            0
        };
        assert_eq!(
            after_exec.get(service_index).map(String::as_str),
            Some("app")
        );
    }

    #[test]
    fn unsupported_compose_preflight_refuses_before_runner() {
        let compose = crate::sandbox_compose::ComposeSpec {
            files: vec!["compose.yml".into()],
            service: Some("app".into()),
            run_services: Vec::new(),
        };
        let mut spec = base_spec(Backend::Apple);
        spec.compose = Some(compose.encode());
        let called = std::cell::Cell::new(false);
        let result = preflight_exec_with(&spec, &|_| {
            called.set(true);
            Some((true, String::new()))
        });
        assert!(
            result
                .unwrap_err()
                .contains("unsupported for backend 'apple'")
        );
        assert!(!called.get());
    }

    #[test]
    fn compose_service_failures_are_classified() {
        assert_eq!(
            compose_exec_failure("no such service: api"),
            Some("Compose preflight failed: service was not found".into())
        );
        assert_eq!(
            compose_exec_failure("service \"api\" is not running"),
            Some("Compose preflight failed: service has no running container".into())
        );
        assert_eq!(compose_exec_failure("permission denied"), None);
    }
}
