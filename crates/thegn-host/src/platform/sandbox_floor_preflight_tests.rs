//! Actual preparation fallthrough with an owned, non-executing OCI adapter.

use std::os::unix::fs::PermissionsExt;
use thegn_core::config::{
    Config, DevcontainerMode, FileAccess, IsolationFloor, OnFloorMiss, SandboxBackend,
    SandboxConfig,
};
use thegn_core::remote::GitLoc;

const IMAGE: &str = "fixture.invalid/the617:inert";
const PREFLIGHT_FAILURE: &str = "THE617_OWNED_EXEC_PREFLIGHT_FAILURE";
const INSPECT_FORMAT: &str = "{{if .State.Running}}RUNNING{{end}}\n{{range .Mounts}}{{if eq .Type \"bind\"}}{{.Source}}\n{{end}}{{end}}";

/// Availability caches otherwise outlive PATH restoration. Clear both sides of
/// the private invocation while the existing environment guard is held.
struct ProbeCache;
impl ProbeCache {
    fn new() -> Self {
        thegn_core::sandbox_backend::clear_probe_cache();
        Self
    }
}
impl Drop for ProbeCache {
    fn drop(&mut self) {
        thegn_core::sandbox_backend::clear_probe_cache();
    }
}

#[test]
fn stronger_runtime_preflight_failure_rechecks_final_host_floor() {
    for (backend, floor, policy) in [
        (
            SandboxBackend::Auto,
            IsolationFloor::SharedKernel,
            OnFloorMiss::Fail,
        ),
        (
            SandboxBackend::Auto,
            IsolationFloor::SharedKernel,
            OnFloorMiss::Degrade,
        ),
        (SandboxBackend::Auto, IsolationFloor::Off, OnFloorMiss::Fail),
        (
            SandboxBackend::Podman,
            IsolationFloor::SharedKernel,
            OnFloorMiss::Fail,
        ),
    ] {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path();
        let worktree = root.join("worktree");
        let bin = root.join("bin");
        let state = root.join("state");
        let config = root.join("config");
        let runtime = root.join("runtime");
        let cache = root.join("cache");
        for path in [&worktree, &bin, &state, &config, &runtime, &cache] {
            std::fs::create_dir(path).unwrap();
        }
        let worktree_text = worktree.to_str().unwrap();
        let name = thegn_core::sandbox::container_name_with_profile(worktree_text, Some("default"));
        let journal = root.join("commands");
        let q = thegn_core::util::sh_quote;
        // No forwarded command is ever executed. Four complete argv shapes are
        // admitted, with all image/name/format/body values pinned to the fixture.
        // Even accidental create/pull/remove/interactive-exec requests fail.
        let script = format!(
            r#"#!/bin/sh
set -eu
journal={journal}
if [ "$#" -eq 1 ] && [ "$1" = version ]; then
    printf 'version\n' >> "$journal"
    printf 'fixture runtime available\n'
elif [ "$#" -eq 3 ] && [ "$1" = image ] && [ "$2" = exists ] && [ "$3" = {image} ]; then
    printf 'image-present\n' >> "$journal"
elif [ "$#" -eq 5 ] && [ "$1" = container ] && [ "$2" = inspect ] && [ "$3" = --format ] && [ "$4" = {inspect} ] && [ "$5" = {name} ]; then
    printf 'ensure-running\n' >> "$journal"
    printf 'RUNNING\n'
elif [ "$#" -eq 5 ] && [ "$1" = exec ] && [ "$2" = {name} ] && [ "$3" = /bin/sh ] && [ "$4" = -lc ] && [ "$5" = true ]; then
    printf 'preflight-failed\n' >> "$journal"
    printf '%s\n' {failure} >&2
    exit 37
else
    printf 'UNEXPECTED\n' >> "$journal"
    exit 98
fi
"#,
            journal = q(journal.to_str().unwrap()),
            image = q(IMAGE),
            inspect = q(INSPECT_FORMAT),
            name = q(&name),
            failure = q(PREFLIGHT_FAILURE),
        );
        let executable = bin.join("podman");
        std::fs::write(&executable, script).unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let shell_env = root.join("empty-shell-env");
        std::fs::write(&shell_env, "").unwrap();
        let env = crate::testenv::EnvVarGuard::set(&[
            ("PATH", bin.to_str().unwrap()),
            ("XDG_STATE_HOME", state.to_str().unwrap()),
            ("XDG_CONFIG_HOME", config.to_str().unwrap()),
            ("XDG_CACHE_HOME", cache.to_str().unwrap()),
            ("XDG_RUNTIME_DIR", runtime.to_str().unwrap()),
            ("THEGN_DIR", runtime.to_str().unwrap()),
            ("ENV", shell_env.to_str().unwrap()),
            ("BASH_ENV", shell_env.to_str().unwrap()),
        ]);
        let probes = ProbeCache::new();
        let mut cfg = Config {
            profile: "default".into(),
            sandbox: SandboxConfig {
                enabled: true,
                backend,
                backend_chain: vec!["podman-rootless".into()],
                image: IMAGE.into(),
                file_access: FileAccess::None,
                auto_caches: false,
                mounts: Vec::new(),
                env_passthrough: Vec::new(),
                devcontainer: DevcontainerMode::Off,
                isolation_floor: floor,
                on_floor_miss: policy,
                ..Default::default()
            },
            ..Default::default()
        };
        cfg.env.clear();
        cfg.placement.enabled = false;
        cfg.daemon.enabled = false;
        let loc = GitLoc::for_worktree(&worktree);
        let result =
            crate::agent::prepare_sandbox_env(&cfg, root, worktree_text, &loc, None, false, None);
        assert_eq!(
            std::fs::read_to_string(&journal).unwrap(),
            "version\nimage-present\nensure-running\npreflight-failed\n",
            "must resolve the stronger candidate, pass its floor and ensure, then fail only exec preflight: {backend:?}/{floor:?}/{policy:?}"
        );
        if backend == SandboxBackend::Podman {
            let error = result.unwrap_err().to_string();
            assert!(error.contains(PREFLIGHT_FAILURE), "{error}");
            assert!(
                !error.contains("host-process"),
                "explicit choice must not fall through: {error}"
            );
        } else if floor == IsolationFloor::SharedKernel && policy == OnFloorMiss::Fail {
            let error = result.unwrap_err().to_string();
            assert!(error.contains("isolation floor"), "{error}");
            assert!(error.contains("host-process"), "{error}");
        } else {
            let outcome = result.unwrap();
            assert!(outcome.spec.is_none());
            assert_eq!(outcome.backend_label, "host");
            assert!(
                outcome
                    .warnings
                    .iter()
                    .any(|w| w.contains(PREFLIGHT_FAILURE))
            );
            let missed = outcome
                .warnings
                .iter()
                .any(|w| w.contains("isolation floor") && w.contains("host-process"));
            assert_eq!(missed, floor == IsolationFloor::SharedKernel);
        }
        drop(probes);
        drop(env);
        fixture
            .close()
            .expect("remove the owned OCI preparation fixture");
    }
}
