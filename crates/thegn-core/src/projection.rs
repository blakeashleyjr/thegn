//! Worktree **projection** — the pure plan for the `data` axis of a named
//! `Environment`: how the worktree's files are made
//! available where the env runs.
//!
//! This mirrors the VPN split: the testable plan lives here in core; the *action*
//! (mount / sync / unmount, which shells out to `sshfs`/`rsync`/a provider file
//! API) lives in `thegn-svc::projection`. A [`ProjectionSpec`] is assembled
//! from a resolved [`Environment`] by [`for_environment`]; the svc layer dispatches
//! on its [`mode`](ProjectionSpec::mode) to the right backend.
//!
//! For the default `in_env` data mode (and `local_exec`) there is nothing to
//! project — the placement already lands the process in the worktree — so
//! [`for_environment`] returns `None` and the lifecycle is a no-op. Only the
//! mounting/syncing modes (`sshfs`, and `sync` once it lands) yield a spec.

use crate::config::DataMode;
use crate::env::Environment;
use crate::placement::Placement;

/// A resolved plan for projecting one worktree into its execution environment.
/// Pure data; the bring-up/teardown lives in `thegn-svc::projection`.
#[derive(Debug, Clone)]
pub struct ProjectionSpec {
    /// The data mode this plan realizes (`Sshfs`, later `Sync`).
    pub mode: DataMode,
    /// The placement — carries the ssh knobs (host/port/identity/jump) the
    /// mounting backends need.
    pub placement: Placement,
    /// The remote tree to project (the sshfs source / sync peer).
    pub remote_dir: String,
    /// A stable local mountpoint under `<thegn_dir>/mounts` (the sshfs/sync
    /// target, and the cwd a pane should use once projected).
    pub mountpoint: String,
}

/// A stable local mountpoint for a `host:remote_path` pair, under
/// `<thegn_dir>/mounts/<slug>`. Deterministic so mount and unmount agree
/// without persisting state.
pub fn mountpoint(host: &str, remote_path: &str) -> String {
    let slug = crate::util::slugify(&format!("{host}-{remote_path}"));
    crate::util::thegn_dir()
        .join("mounts")
        .join(slug)
        .to_string_lossy()
        .into_owned()
}

/// Build the projection plan for a resolved environment, or `None` when the data
/// mode needs no projection (`in_env`/`local_exec`) or the inputs are incomplete
/// (e.g. `sshfs` without an ssh placement or a `remote_dir`).
pub fn for_environment(env: &Environment) -> Option<ProjectionSpec> {
    match env.data {
        // Both project a remote tree to a local mountpoint over ssh.
        DataMode::Sshfs | DataMode::Sync => {
            let Placement::Ssh(s) = &env.placement else {
                return None;
            };
            let remote_dir = env.sandbox.remote.remote_dir.trim().to_string();
            if remote_dir.is_empty() {
                return None;
            }
            let mountpoint = mountpoint(&s.host, &remote_dir);
            Some(ProjectionSpec {
                mode: env.data,
                placement: env.placement.clone(),
                remote_dir,
                mountpoint,
            })
        }
        DataMode::InEnv | DataMode::LocalExec => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SandboxConfig;
    use crate::placement::{SshPlacement, TransportKind};

    fn ssh_env(data: DataMode, remote_dir: &str) -> Environment {
        let mut sandbox = SandboxConfig::default();
        sandbox.remote.remote_dir = remote_dir.to_string();
        Environment {
            name: "remote".into(),
            placement: Placement::Ssh(SshPlacement::plain(
                "dev-box".into(),
                22,
                false,
                TransportKind::Ssh,
            )),
            sandbox,
            data,
        }
    }

    #[test]
    fn in_env_needs_no_projection() {
        let env = ssh_env(DataMode::InEnv, "/srv/work");
        assert!(for_environment(&env).is_none());
    }

    #[test]
    fn local_exec_needs_no_projection() {
        let env = ssh_env(DataMode::LocalExec, "/srv/work");
        assert!(for_environment(&env).is_none());
    }

    #[test]
    fn sshfs_without_remote_dir_is_none() {
        let env = ssh_env(DataMode::Sshfs, "   ");
        assert!(for_environment(&env).is_none());
    }

    #[test]
    fn sshfs_without_ssh_placement_is_none() {
        let mut env = ssh_env(DataMode::Sshfs, "/srv/work");
        env.placement = Placement::Local;
        assert!(for_environment(&env).is_none());
    }

    #[test]
    fn sshfs_yields_spec_with_stable_mountpoint() {
        let env = ssh_env(DataMode::Sshfs, "/srv/work");
        let spec = for_environment(&env).expect("sshfs spec");
        assert_eq!(spec.mode, DataMode::Sshfs);
        assert_eq!(spec.remote_dir, "/srv/work");
        // Deterministic: recomputing from the same inputs matches.
        assert_eq!(spec.mountpoint, mountpoint("dev-box", "/srv/work"));
        assert!(spec.mountpoint.contains("mounts"));
    }
}
