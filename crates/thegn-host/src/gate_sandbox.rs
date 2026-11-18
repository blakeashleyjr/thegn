//! Where the merge gate's commands run (THE-2200): `[merge_queue]
//! gate_sandbox = "off"` keeps today's host gate; `"sealed"` runs
//! `gate_setup_command`, `gate_command` and `regenerate_command` through the
//! repo's sealed sandbox.
//!
//! A sealed gate never falls back to the host: when the sealed spec cannot be
//! resolved (no runtime, a profile below the sealed floor, an unreadable
//! config) the gate is an infrastructure hold (`GateVerdict::Error`), never a
//! host run and never branch blame.
//!
//! What a sealed gate may touch:
//! - writable: the gate checkout (`spec.worktree`) and its `CARGO_TARGET_DIR`;
//! - read-only: the repo's git common dir (the checkout's `.git` points into it);
//! - the profile's own read-only root and mounts.
//!
//! The guarded entry masks the daemon socket, state run dir, runtime dir and
//! Lead control dir; the sealed floor hides `$HOME`.

use std::path::Path;

use thegn_core::config::{Config, GateSandbox, MergeQueueConfig, Network};
use thegn_core::sandbox::{Mount, SandboxSpec, SecretEnvFile};

/// The resolve error when the floor gate refuses without a reason of its own.
const NEEDS_SEALED: &str = "the merge gate needs a sealed sandbox for this repo";

/// How a gate command is executed.
pub(crate) enum GateExec {
    /// On the host (the `off` default).
    Host,
    /// Inside this sealed sandbox.
    Sealed(Box<SandboxSpec>),
}

impl GateExec {
    /// The first line of a gate log for this mode (sealed runs only; the host
    /// gate keeps `gate::HOST_EXECUTION_LOG_HEADER`).
    pub(crate) fn sealed_log_header(spec: &SandboxSpec) -> String {
        format!("sandboxed gate (network: {})\n", spec.network.as_str())
    }
}

/// Resolve the gate execution for `repo_root` from the full config: the
/// repo's effective `[merge_queue] gate_sandbox` (a repo overlay can only
/// raise it) decides the mode; `sealed` must pass the same sealed floor an
/// unattended validation does.
pub(crate) fn resolve(full: &Config, repo_root: &Path) -> Result<GateExec, String> {
    match full.repo_merge_queue(repo_root).gate_sandbox {
        GateSandbox::Off => Ok(GateExec::Host),
        GateSandbox::Sealed => sealed_spec(full, repo_root),
    }
}

/// The sealed spec for `repo_root`, or the refusal. Blocking (it ensures the
/// sandbox and runs a preflight): call off the event loop.
fn sealed_spec(full: &Config, repo_root: &Path) -> Result<GateExec, String> {
    use crate::agent_run::AgentDispatch;
    use crate::daemon::stage_executor::{floor_refusal, host_validation_isolated};
    let Some(root) = repo_root.to_str() else {
        return Err(format!("{NEEDS_SEALED} (repo path is not UTF-8)"));
    };
    let gate = crate::agent_run::agent_floor_gate(
        full,
        root,
        true,
        thegn_core::config::IsolationFloor::SharedKernel,
        thegn_core::config::OnFloorMiss::Fail,
    );
    match gate {
        AgentDispatch::Run(Some(spec)) if host_validation_isolated(&spec) => {
            Ok(GateExec::Sealed(Box::new(spec)))
        }
        other => Err(floor_refusal(other).unwrap_or_else(|| NEEDS_SEALED.to_owned())),
    }
}

/// Resolve the gate execution from the repo's effective `[merge_queue]`
/// (`mq`, what the gate was handed). `off` never loads config; `sealed` loads
/// the process's effective config (the gate runs off-loop on its worker) and
/// resolves the sealed spec. Any error is an `Err`: the caller holds the gate,
/// it never runs on the host.
pub(crate) fn resolve_for_gate(
    repo_root: &Path,
    mq: &MergeQueueConfig,
) -> Result<GateExec, String> {
    resolve_mode(repo_root, mq.gate_sandbox)
}

/// [`resolve_for_gate`] from the mode alone (the fold's `regenerate_command`
/// carries only the mode, not the whole `[merge_queue]`).
pub(crate) fn resolve_mode(repo_root: &Path, mode: GateSandbox) -> Result<GateExec, String> {
    resolve_for_gate_with(repo_root, mode, load_process_config, sealed_spec)
}

fn load_process_config() -> Result<Config, String> {
    Config::try_load_layered(&thegn_core::config::ProcessEnv, &[], None)
        .map_err(|error| format!("the sealed merge gate cannot load the config: {error}"))
}

/// [`resolve_for_gate`] with the config loader and sealed resolver injected
/// (tests prove `off` never loads config). `mode` is authoritative: a
/// `sealed` gate stays sealed even if the reloaded config reads `off`.
fn resolve_for_gate_with(
    repo_root: &Path,
    mode: GateSandbox,
    load: impl FnOnce() -> Result<Config, String>,
    sealed: impl FnOnce(&Config, &Path) -> Result<GateExec, String>,
) -> Result<GateExec, String> {
    match mode {
        GateSandbox::Off => Ok(GateExec::Host),
        GateSandbox::Sealed => {
            let full = load()?;
            sealed(&full, repo_root)
        }
    }
}

/// Whether `path` is `root` or lies beneath it (component-wise).
fn under(path: &Path, root: &Path) -> bool {
    !root.as_os_str().is_empty() && path.starts_with(root)
}

/// The sealed spec for one gate command: `spec` re-rooted at `checkout`, with
/// the target dir bound read-write (unless it is inside the checkout) and the
/// git common dir bound read-only (unless a read-only mount already covers
/// it). Pure.
pub(crate) fn gate_spec(
    spec: &SandboxSpec,
    checkout: &Path,
    target: Option<&Path>,
    common_git: &Path,
    extra_env: &[(String, String)],
) -> SandboxSpec {
    let mut spec = spec.clone();
    spec.worktree = checkout.to_path_buf();
    if let Some(target) = target
        && !under(target, checkout)
    {
        let target = target.to_string_lossy().into_owned();
        spec.mounts.push(Mount {
            host: target.clone(),
            dest: target,
            ro: false,
            cache: false,
        });
    }
    // A covering read-write mount does not count: the nested read-only bind
    // is pushed last, so it overlays it.
    let common_covered = spec
        .mounts
        .iter()
        .any(|mount| mount.ro && under(common_git, Path::new(&mount.host)));
    if !common_covered {
        let common = common_git.to_string_lossy().into_owned();
        spec.mounts.push(Mount {
            host: common.clone(),
            dest: common,
            ro: true,
            cache: false,
        });
    }
    spec.env.extend(extra_env.iter().cloned());
    if spec.network == Network::None {
        spec.env
            .push(("CARGO_NET_OFFLINE".to_owned(), "true".to_owned()));
    }
    spec
}

/// The full argv for one sealed gate command: [`gate_spec`] through the
/// guarded sandbox entry (control-dir masks, secret env-file) and the
/// `thegn.slice` cap tagged as a gate scope. The returned env-file guard must
/// stay alive until the child is reaped.
pub(crate) fn sealed_argv(
    spec: &SandboxSpec,
    checkout: &Path,
    target: Option<&Path>,
    common_git: &Path,
    command: &str,
    extra_env: &[(String, String)],
) -> Result<(Vec<String>, Option<SecretEnvFile>), String> {
    let spec = gate_spec(spec, checkout, target, common_git, extra_env);
    let (inner, env_file) = thegn_core::sandbox::enter_argv_guarded(&spec, command)
        .map_err(|error| format!("the sealed merge gate cannot start: {error}"))?
        .into_parts();
    let argv = thegn_core::sandbox_cpucap::wrap_background_argv_tagged(
        inner,
        &thegn_core::sandbox_cpucap::ScopeTag {
            kind: thegn_core::sandbox_cpucap::ScopeKind::Gate,
            worktree: checkout.to_string_lossy().into_owned(),
        },
    );
    if argv.is_empty() {
        return Err("the sealed merge gate cannot start: empty sandbox entry".to_owned());
    }
    Ok((argv, env_file))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::path::PathBuf;

    fn env(spec: &SandboxSpec, key: &str) -> Option<String> {
        spec.env
            .iter()
            .rev()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
    }

    fn gate_env() -> Vec<(String, String)> {
        vec![
            ("CARGO_TARGET_DIR".into(), "/state/gate/target".into()),
            ("THEGN_GATE".into(), "1".into()),
            ("THEGN_WORKTREE".into(), "/state/gate/wt".into()),
            ("THEGN_GATE_OID".into(), "a".repeat(40)),
            ("GIT_NO_REPLACE_OBJECTS".into(), "1".into()),
        ]
    }

    fn base() -> SandboxSpec {
        SandboxSpec {
            worktree: PathBuf::from("/repo"),
            network: Network::None,
            ..SandboxSpec::default()
        }
    }

    #[test]
    fn the_gate_checkout_is_the_sandbox_worktree() {
        let spec = gate_spec(
            &base(),
            Path::new("/state/gate/wt"),
            Some(Path::new("/state/gate/target")),
            Path::new("/repo/.git"),
            &gate_env(),
        );
        assert_eq!(spec.worktree, PathBuf::from("/state/gate/wt"));
    }

    #[test]
    fn the_target_is_bound_rw_and_the_common_git_ro() {
        let spec = gate_spec(
            &base(),
            Path::new("/state/gate/wt"),
            Some(Path::new("/state/gate/target")),
            Path::new("/repo/.git"),
            &gate_env(),
        );
        let target = spec
            .mounts
            .iter()
            .find(|m| m.host == "/state/gate/target")
            .expect("target bind");
        assert!(!target.ro);
        assert_eq!(target.dest, "/state/gate/target");
        let common = spec
            .mounts
            .iter()
            .find(|m| m.host == "/repo/.git")
            .expect("common git bind");
        assert!(common.ro);
        assert_eq!(common.dest, "/repo/.git");
        // Nothing else became writable: exactly one rw bind (the target).
        assert_eq!(spec.mounts.iter().filter(|m| !m.ro).count(), 1);
    }

    #[test]
    fn a_target_under_the_checkout_gets_no_second_bind() {
        let spec = gate_spec(
            &base(),
            Path::new("/state/gate/wt"),
            Some(Path::new("/state/gate/wt/target")),
            Path::new("/repo/.git"),
            &gate_env(),
        );
        assert!(!spec.mounts.iter().any(|m| m.host.ends_with("target")));
        assert_eq!(spec.mounts.len(), 1, "only the common git bind");
    }

    #[test]
    fn a_read_only_mount_covering_the_common_git_is_not_duplicated() {
        let mut spec = base();
        spec.mounts.push(Mount {
            host: "/repo".into(),
            dest: "/repo".into(),
            ro: true,
            cache: false,
        });
        let spec = gate_spec(
            &spec,
            Path::new("/state/gate/wt"),
            None,
            Path::new("/repo/.git"),
            &gate_env(),
        );
        assert_eq!(spec.mounts.len(), 1);
    }

    #[test]
    fn a_read_write_mount_covering_the_common_git_still_gets_a_ro_overlay() {
        let mut spec = base();
        spec.mounts.push(Mount {
            host: "/repo".into(),
            dest: "/repo".into(),
            ro: false,
            cache: false,
        });
        let spec = gate_spec(
            &spec,
            Path::new("/state/gate/wt"),
            None,
            Path::new("/repo/.git"),
            &gate_env(),
        );
        let last = spec.mounts.last().unwrap();
        assert_eq!(last.host, "/repo/.git");
        assert!(last.ro);
    }

    #[test]
    fn cargo_net_offline_only_without_network() {
        let none = gate_spec(
            &base(),
            Path::new("/wt"),
            None,
            Path::new("/repo/.git"),
            &gate_env(),
        );
        assert_eq!(env(&none, "CARGO_NET_OFFLINE").as_deref(), Some("true"));
        let nat = gate_spec(
            &SandboxSpec {
                network: Network::Nat,
                ..base()
            },
            Path::new("/wt"),
            None,
            Path::new("/repo/.git"),
            &gate_env(),
        );
        assert_eq!(env(&nat, "CARGO_NET_OFFLINE"), None);
    }

    #[test]
    fn the_gate_env_reaches_the_sandbox() {
        let spec = gate_spec(
            &base(),
            Path::new("/wt"),
            None,
            Path::new("/repo/.git"),
            &gate_env(),
        );
        assert_eq!(env(&spec, "THEGN_GATE_OID"), Some("a".repeat(40)));
        assert_eq!(env(&spec, "THEGN_GATE").as_deref(), Some("1"));
        assert_eq!(
            env(&spec, "CARGO_TARGET_DIR").as_deref(),
            Some("/state/gate/target")
        );
        assert_eq!(env(&spec, "GIT_NO_REPLACE_OBJECTS").as_deref(), Some("1"));
    }

    #[test]
    fn the_sealed_log_header_names_the_network() {
        assert_eq!(
            GateExec::sealed_log_header(&base()),
            "sandboxed gate (network: none)\n"
        );
        let nat = SandboxSpec {
            network: Network::Nat,
            ..base()
        };
        assert_eq!(
            GateExec::sealed_log_header(&nat),
            "sandboxed gate (network: nat)\n"
        );
    }

    #[test]
    fn off_never_loads_config() {
        let loaded = Cell::new(false);
        let resolved = resolve_for_gate_with(
            Path::new("/nonexistent"),
            MergeQueueConfig::default().gate_sandbox,
            || {
                loaded.set(true);
                Ok(Config::default())
            },
            |_, _| panic!("off never resolves a sealed spec"),
        );
        assert!(matches!(resolved, Ok(GateExec::Host)));
        assert!(!loaded.get());
    }

    #[test]
    fn sealed_loads_config_and_never_falls_back_to_the_host() {
        let sealed = GateSandbox::Sealed;
        let loaded = Cell::new(false);
        // The reloaded config reads `off`, yet the gate's own `sealed` holds.
        let resolved = resolve_for_gate_with(
            Path::new("/nonexistent"),
            sealed,
            || {
                loaded.set(true);
                Ok(Config::default())
            },
            |_, _| Err("no runtime".into()),
        );
        assert!(loaded.get());
        assert_eq!(resolved.err().as_deref(), Some("no runtime"));

        let unreadable = resolve_for_gate_with(
            Path::new("/nonexistent"),
            sealed,
            || Err("bad toml".into()),
            |_, _| panic!("no spec without a config"),
        );
        assert_eq!(unreadable.err().as_deref(), Some("bad toml"));
    }

    #[test]
    fn resolve_follows_the_full_config_mode() {
        let root = Path::new("/nonexistent");
        assert!(matches!(
            resolve(&Config::default(), root),
            Ok(GateExec::Host)
        ));
    }
}
