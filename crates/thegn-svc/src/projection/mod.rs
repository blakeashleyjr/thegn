//! Worktree **projection** backends — realize the `data` axis of a named
//! environment: make the worktree available where the env runs and keep it
//! coherent. The pure plan (`ProjectionSpec`)
//! is built in core; this layer runs/tears it down, mirroring the vpn seam
//! (core `VpnSpec` / svc `VpnProvider`).
//!
//! Division of labor: `Bind`/`in_env`/`local_exec` need no active projection —
//! the placement or the OCI bind-mount already lands the process in the worktree,
//! so their methods are no-ops. `Sshfs` runs the FUSE mount/unmount (reusing the
//! argv builders on `SshPlacement`). The
//! `sync` mode (changed-files manifest, for file-API-only providers) plugs in here
//! in the sync phase.
//!
//! The subprocess execution is the I/O seam (exercised end-to-end by
//! `test/smoke.sh` / `thegn env up`); the pure plan it consumes is unit-tested
//! in core.

use anyhow::{Result, bail};
use thegn_core::config::DataMode;
use thegn_core::placement::{Placement, SshPlacement};
use thegn_core::projection::ProjectionSpec;

/// The result of projecting a worktree.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Mounted {
    /// The local path a pane should use as its cwd, when the projection relocated
    /// the tree (the sshfs/sync mountpoint). `None` for bind/in-env, where the
    /// placement already cd's into the worktree.
    pub local_cwd: Option<String>,
}

/// One projection backend: bring the worktree's files where the env runs, refresh
/// the delta (sync modes), and tear the projection down.
pub trait ProjectionBackend: Send + Sync {
    fn kind(&self) -> &'static str;
    /// Make the worktree available. Idempotent where the underlying tool allows.
    fn mount(&self, spec: &ProjectionSpec) -> Result<Mounted>;
    /// Push/pull the changed-files delta (sync modes). A no-op for live mounts
    /// (bind/sshfs), which stay coherent without an explicit step.
    fn refresh(&self, spec: &ProjectionSpec) -> Result<()>;
    /// Tear the projection down (unmount / final sync).
    fn unmount(&self, spec: &ProjectionSpec) -> Result<()>;
}

/// Resolve the projection backend for a spec. Returns a concrete dispatcher
/// (the same concrete-wrapper + `match` pattern as `vpn::for_provider`), not a
/// boxed trait object.
pub fn for_data_mode(spec: &ProjectionSpec) -> BuiltinProjection<'_> {
    BuiltinProjection { spec }
}

/// The built-in projection dispatcher: switches on [`ProjectionSpec::mode`].
pub struct BuiltinProjection<'a> {
    spec: &'a ProjectionSpec,
}

impl ProjectionBackend for BuiltinProjection<'_> {
    fn kind(&self) -> &'static str {
        match self.spec.mode {
            DataMode::Sshfs => "sshfs",
            DataMode::Sync => "sync",
            DataMode::LocalExec => "local_exec",
            DataMode::InEnv => "bind",
        }
    }

    fn mount(&self, spec: &ProjectionSpec) -> Result<Mounted> {
        match spec.mode {
            DataMode::Sshfs => mount_sshfs(spec),
            // Initial pull of the remote tree into the local working copy.
            DataMode::Sync => mount_sync(spec),
            // in_env/local_exec: the placement/bind-mount already provides the tree.
            DataMode::InEnv | DataMode::LocalExec => Ok(Mounted::default()),
        }
    }

    fn refresh(&self, spec: &ProjectionSpec) -> Result<()> {
        match spec.mode {
            // Push the local delta back to the remote. (sshfs/bind are live mounts.)
            DataMode::Sync => push_sync(spec),
            DataMode::Sshfs | DataMode::InEnv | DataMode::LocalExec => Ok(()),
        }
    }

    fn unmount(&self, spec: &ProjectionSpec) -> Result<()> {
        match spec.mode {
            DataMode::Sshfs => unmount_sshfs(spec),
            // Final push so local edits are not lost on close.
            DataMode::Sync => push_sync(spec),
            DataMode::InEnv | DataMode::LocalExec => Ok(()),
        }
    }
}

fn mount_sshfs(spec: &ProjectionSpec) -> Result<Mounted> {
    let Placement::Ssh(s) = &spec.placement else {
        bail!("sshfs projection requires an ssh placement");
    };
    std::fs::create_dir_all(&spec.mountpoint).ok();
    let argv = s.sshfs_mount_argv(&spec.remote_dir, &spec.mountpoint);
    run(&argv)?;
    Ok(Mounted {
        local_cwd: Some(spec.mountpoint.clone()),
    })
}

fn unmount_sshfs(spec: &ProjectionSpec) -> Result<()> {
    let argv = SshPlacement::sshfs_unmount_argv(&spec.mountpoint);
    run(&argv)
}

/// `rsync -e` transport flag carrying the placement's ssh knobs (port/identity).
fn rsync_ssh_opt(s: &SshPlacement) -> Vec<String> {
    let mut ssh = String::from("ssh");
    if s.port != 22 {
        ssh.push_str(&format!(" -p {}", s.port));
    }
    if let Some(id) = &s.identity {
        ssh.push_str(&format!(" -i {id}"));
    }
    vec!["-e".into(), ssh]
}

/// `host:remote_dir/` (trailing slash → sync directory *contents*).
fn rsync_remote(s: &SshPlacement, remote_dir: &str) -> String {
    format!("{}:{}/", s.host, remote_dir.trim_end_matches('/'))
}

/// Local working-copy dir as `path/` (trailing slash, contents).
fn local_dir(mountpoint: &str) -> String {
    format!("{}/", mountpoint.trim_end_matches('/'))
}

/// Reference `sync` projection: initial **pull** of the remote tree into the
/// local working copy (`--delete` mirrors the remote). A changed-files manifest
/// is what `rsync` computes internally; provider file-API adapters override this.
/// One-direction-each (pull on mount, push on refresh/close) — true bidirectional
/// conflict resolution is out of scope for the reference engine.
fn mount_sync(spec: &ProjectionSpec) -> Result<Mounted> {
    let Placement::Ssh(s) = &spec.placement else {
        bail!("sync projection requires an ssh placement");
    };
    std::fs::create_dir_all(&spec.mountpoint).ok();
    let mut argv = vec!["rsync".into(), "-az".into(), "--delete".into()];
    argv.extend(rsync_ssh_opt(s));
    argv.push(rsync_remote(s, &spec.remote_dir));
    argv.push(local_dir(&spec.mountpoint));
    run(&argv)?;
    Ok(Mounted {
        local_cwd: Some(spec.mountpoint.clone()),
    })
}

/// Push the local working copy back to the remote (no `--delete`: never reaches
/// in and removes remote files the local copy lacks).
fn push_sync(spec: &ProjectionSpec) -> Result<()> {
    let Placement::Ssh(s) = &spec.placement else {
        bail!("sync projection requires an ssh placement");
    };
    let mut argv = vec!["rsync".into(), "-az".into()];
    argv.extend(rsync_ssh_opt(s));
    argv.push(local_dir(&spec.mountpoint));
    argv.push(rsync_remote(s, &spec.remote_dir));
    run(&argv)
}

/// Run an argv to completion, mapping a spawn error or non-zero exit to `Err`.
fn run(argv: &[String]) -> Result<()> {
    if argv.is_empty() {
        bail!("empty projection argv");
    }
    let status = std::process::Command::new(&argv[0])
        .args(&argv[1..])
        .status()
        .map_err(|e| anyhow::anyhow!("could not run {}: {e}", argv[0]))?;
    if !status.success() {
        bail!("{} exited with {status}", argv[0]);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use thegn_core::placement::{SshPlacement, TransportKind};

    fn spec(mode: DataMode, placement: Placement) -> ProjectionSpec {
        ProjectionSpec {
            mode,
            placement,
            remote_dir: "/srv/work".into(),
            mountpoint: "/tmp/sz-mount".into(),
        }
    }
    fn ssh() -> Placement {
        Placement::Ssh(SshPlacement::plain(
            "dev-box".into(),
            22,
            false,
            TransportKind::Ssh,
        ))
    }

    #[test]
    fn kind_dispatch_matches_mode() {
        assert_eq!(for_data_mode(&spec(DataMode::Sshfs, ssh())).kind(), "sshfs");
        assert_eq!(for_data_mode(&spec(DataMode::Sync, ssh())).kind(), "sync");
        assert_eq!(
            for_data_mode(&spec(DataMode::InEnv, Placement::Local)).kind(),
            "bind"
        );
        assert_eq!(
            for_data_mode(&spec(DataMode::LocalExec, Placement::Local)).kind(),
            "local_exec"
        );
    }

    #[test]
    fn sync_without_ssh_placement_errors() {
        let s = spec(DataMode::Sync, Placement::Local);
        let b = for_data_mode(&s);
        assert!(b.mount(&s).is_err());
        assert!(b.refresh(&s).is_err());
    }

    #[test]
    fn rsync_builders_carry_ssh_knobs_and_trailing_slashes() {
        use thegn_core::placement::SshPlacement;
        let mut p = SshPlacement::plain("dev-box".into(), 2222, false, TransportKind::Ssh);
        p.identity = Some("/k/id".into());
        assert_eq!(rsync_ssh_opt(&p), vec!["-e", "ssh -p 2222 -i /k/id"]);
        assert_eq!(rsync_remote(&p, "/srv/work/"), "dev-box:/srv/work/");
        assert_eq!(local_dir("/tmp/m/"), "/tmp/m/");
        // Default port omits -p.
        let q = SshPlacement::plain("h".into(), 22, false, TransportKind::Ssh);
        assert_eq!(rsync_ssh_opt(&q), vec!["-e", "ssh"]);
    }

    #[test]
    fn in_env_mount_unmount_are_noops() {
        let s = spec(DataMode::InEnv, Placement::Local);
        let b = for_data_mode(&s);
        assert_eq!(b.mount(&s).unwrap(), Mounted::default());
        assert!(b.refresh(&s).is_ok());
        assert!(b.unmount(&s).is_ok());
    }

    #[test]
    fn sshfs_without_ssh_placement_errors() {
        let s = spec(DataMode::Sshfs, Placement::Local);
        let b = for_data_mode(&s);
        assert!(b.mount(&s).is_err());
    }
}
