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
    if !s.extra_args.is_empty() {
        bail!("sshfs projection does not support ssh extra_args");
    }
    if let Err(error) = std::fs::create_dir_all(&spec.mountpoint) {
        tracing::debug!(%error, path = %spec.mountpoint, "could not create sshfs projection mountpoint");
    }
    let argv = sshfs_mount_argv_for(s, &spec.remote_dir, &spec.mountpoint);
    run(&argv)?;
    Ok(Mounted {
        local_cwd: Some(spec.mountpoint.clone()),
    })
}

fn sshfs_mount_argv_for(s: &SshPlacement, remote: &str, mountpoint: &str) -> Vec<String> {
    let mut argv = s.sshfs_mount_argv(remote, mountpoint);
    if let Some(config) = &s.ssh_config {
        argv.extend(["-F".into(), config.clone()]);
    }
    if let Some(jump_host) = &s.jump_host {
        argv.extend(["-o".into(), format!("ProxyJump={jump_host}")]);
    }
    argv
}

fn unmount_sshfs(spec: &ProjectionSpec) -> Result<()> {
    let candidates = spec.legacy_mountpoints();
    let slug_legacy_path = match &spec.placement {
        Placement::Ssh(ssh) => {
            thegn_core::projection::legacy_mountpoint(&ssh.host, &spec.remote_dir)
        }
        _ => None,
    };
    let legacy_paths: Vec<_> = candidates
        .into_iter()
        .filter(|path| std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_dir()))
        .collect();
    let plan = sshfs_unmount_plan(spec, &legacy_paths);
    let primary_result = run(&plan[0]);

    let mut slug_legacy_succeeded = false;
    let mut v1_legacy_succeeded = false;
    for (legacy_path, legacy_argv) in legacy_paths.iter().zip(plan.iter().skip(1)) {
        match run(legacy_argv) {
            Ok(()) => {
                if slug_legacy_path
                    .as_ref()
                    .is_some_and(|slug_path| slug_path == legacy_path)
                {
                    slug_legacy_succeeded = true;
                } else {
                    v1_legacy_succeeded = true;
                }
                if let Err(error) = std::fs::remove_dir(legacy_path) {
                    tracing::debug!(%error, path = %legacy_path, "could not remove empty legacy sshfs mountpoint");
                }
            }
            Err(error) => {
                tracing::debug!(%error, path = %legacy_path, "could not unmount legacy sshfs mountpoint");
            }
        }
    }

    finish_sshfs_unmount(
        spec,
        primary_result,
        slug_legacy_succeeded,
        v1_legacy_succeeded,
    )
}

fn finish_sshfs_unmount(
    spec: &ProjectionSpec,
    primary_result: Result<()>,
    slug_legacy_succeeded: bool,
    v1_legacy_succeeded: bool,
) -> Result<()> {
    match primary_result {
        Ok(()) => Ok(()),
        Err(error) if slug_legacy_succeeded => {
            tracing::warn!(
                %error,
                mountpoint = %spec.mountpoint,
                "primary sshfs unmount failed but legacy unmount succeeded"
            );
            Ok(())
        }
        Err(error) if v1_legacy_succeeded => {
            tracing::debug!(
                mountpoint = %spec.mountpoint,
                "v1 legacy sshfs unmount succeeded but does not replace primary unmount"
            );
            Err(error)
        }
        Err(error) => Err(error),
    }
}

fn sshfs_unmount_plan(spec: &ProjectionSpec, legacy_paths: &[String]) -> Vec<Vec<String>> {
    let mut plan = vec![SshPlacement::sshfs_unmount_argv(&spec.mountpoint)];
    plan.extend(
        legacy_paths
            .iter()
            .map(|path| SshPlacement::sshfs_unmount_argv(path)),
    );
    plan
}

/// Reap old slug-only SSHFS projection directories once at startup. Unmount is
/// non-lazy and removal only succeeds for an empty directory.
pub fn reap_legacy_mounts(mounts_dir: &std::path::Path) -> usize {
    reap_legacy_mounts_with(mounts_dir, run)
}

fn reap_legacy_mounts_with(
    mounts_dir: &std::path::Path,
    mut run_unmount: impl FnMut(&[String]) -> Result<()>,
) -> usize {
    let entries = match std::fs::read_dir(mounts_dir) {
        Ok(entries) => entries,
        Err(error) => {
            tracing::debug!(%error, path = %mounts_dir.display(), "could not scan legacy projection mounts");
            return 0;
        }
    };
    let mut reaped = 0;
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                tracing::debug!(%error, path = %mounts_dir.display(), "could not read projection mount entry");
                continue;
            }
        };
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if !thegn_core::projection::is_legacy_mount_component(&name) {
            continue;
        }
        let path = entry.path();
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => continue,
            Err(error) => {
                tracing::debug!(%error, path = %path.display(), "could not inspect legacy projection mount");
                continue;
            }
        }
        let path_string = path.to_string_lossy().into_owned();
        match run_unmount(&SshPlacement::sshfs_unmount_argv(&path_string)) {
            Ok(()) => match std::fs::remove_dir(&path) {
                Ok(()) => reaped += 1,
                Err(error) => {
                    tracing::debug!(%error, path = %path.display(), "could not remove empty legacy projection mountpoint")
                }
            },
            Err(error) => {
                tracing::debug!(%error, path = %path.display(), "could not unmount legacy projection mountpoint")
            }
        }
    }
    reaped
}

/// `rsync -e` transport flag carrying the placement's ssh knobs.
fn rsync_ssh_opt(s: &SshPlacement) -> Vec<String> {
    let mut ssh = vec!["ssh".to_string()];
    if s.port != 22 {
        ssh.extend(["-p".into(), s.port.to_string()]);
    }
    if let Some(config) = &s.ssh_config {
        ssh.extend(["-F".into(), config.clone()]);
    }
    if let Some(jump_host) = &s.jump_host {
        ssh.extend(["-J".into(), jump_host.clone()]);
    }
    if let Some(id) = &s.identity {
        ssh.extend(["-i".into(), id.clone()]);
    }
    ssh.extend(s.extra_args.iter().cloned());
    let command = ssh
        .iter()
        .map(|arg| thegn_core::util::sh_quote(arg))
        .collect::<Vec<_>>()
        .join(" ");
    vec!["-e".into(), command]
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
    if let Err(error) = std::fs::create_dir_all(&spec.mountpoint) {
        tracing::debug!(%error, path = %spec.mountpoint, "could not create sync projection mountpoint");
    }
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
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};
    use thegn_core::placement::{SshPlacement, TransportKind};
    use tracing::field::{Field, Visit};
    use tracing::span;
    use tracing::{Event, Level, Metadata, Subscriber};

    #[derive(Clone, Default)]
    struct CapturedWarnings(Arc<Mutex<Vec<BTreeMap<String, String>>>>);

    impl CapturedWarnings {
        fn records(&self) -> Vec<BTreeMap<String, String>> {
            self.0.lock().unwrap().clone()
        }
    }

    struct FieldVisitor(BTreeMap<String, String>);

    impl Visit for FieldVisitor {
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            self.0
                .insert(field.name().to_string(), format!("{value:?}"));
        }

        fn record_str(&mut self, field: &Field, value: &str) {
            self.0.insert(field.name().to_string(), value.to_string());
        }
    }

    impl Subscriber for CapturedWarnings {
        fn enabled(&self, _: &Metadata<'_>) -> bool {
            true
        }

        fn new_span(&self, _: &span::Attributes<'_>) -> span::Id {
            span::Id::from_u64(1)
        }

        fn record(&self, _: &span::Id, _: &span::Record<'_>) {}

        fn record_follows_from(&self, _: &span::Id, _: &span::Id) {}

        fn event(&self, event: &Event<'_>) {
            if event.metadata().level() == &Level::WARN {
                let mut fields = FieldVisitor(BTreeMap::new());
                event.record(&mut fields);
                self.0.lock().unwrap().push(fields.0);
            }
        }

        fn enter(&self, _: &span::Id) {}

        fn exit(&self, _: &span::Id) {}
    }

    fn spec(mode: DataMode, placement: Placement) -> ProjectionSpec {
        let mountpoint = match &placement {
            Placement::Ssh(ssh) => thegn_core::projection::mountpoint(ssh, "/srv/work"),
            _ => "/tmp/tg-mount".into(),
        };
        ProjectionSpec {
            mode,
            placement,
            remote_dir: "/srv/work".into(),
            mountpoint,
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
    fn rsync_ssh_opt_carries_config_and_jump_host() {
        let mut placement = SshPlacement::plain("dev-box".into(), 22, false, TransportKind::Ssh);
        placement.ssh_config = Some("/cfg".into());
        placement.jump_host = Some("bastion".into());

        assert_eq!(
            rsync_ssh_opt(&placement),
            vec!["-e", "ssh -F /cfg -J bastion"]
        );
    }

    #[test]
    fn rsync_ssh_opt_quotes_identity_and_appends_extra_args() {
        let mut placement = SshPlacement::plain("dev-box".into(), 22, false, TransportKind::Ssh);
        placement.identity = Some("/k/my id".into());
        placement.extra_args = vec!["-v".into()];

        assert_eq!(
            rsync_ssh_opt(&placement),
            vec!["-e", "ssh -i '/k/my id' -v"]
        );
    }

    #[test]
    fn sshfs_mount_argv_carries_config_and_jump_host() {
        let mut placement = SshPlacement::plain("dev-box".into(), 22, false, TransportKind::Ssh);
        placement.ssh_config = Some("/cfg".into());
        placement.jump_host = Some("bastion".into());
        let argv = sshfs_mount_argv_for(&placement, "/srv/work", "/tmp/mount");

        assert!(
            argv.windows(2)
                .any(|pair| pair[0] == "-F" && pair[1] == "/cfg")
        );
        assert!(
            argv.windows(2)
                .any(|pair| pair[0] == "-o" && pair[1] == "ProxyJump=bastion")
        );

        let plain = SshPlacement::plain("dev-box".into(), 22, false, TransportKind::Ssh);
        assert_eq!(
            sshfs_mount_argv_for(&plain, "/srv/work", "/tmp/mount"),
            plain.sshfs_mount_argv("/srv/work", "/tmp/mount")
        );
    }

    #[test]
    fn sshfs_extra_args_fail_before_creating_mountpoint_or_spawning() {
        let temp = tempfile::tempdir().unwrap();
        let mut placement = match ssh() {
            Placement::Ssh(placement) => placement,
            _ => unreachable!(),
        };
        placement.extra_args = vec!["-v".into()];
        let mut projection = spec(DataMode::Sshfs, Placement::Ssh(placement));
        let mountpoint = temp.path().join("mount");
        projection.mountpoint = mountpoint.to_str().unwrap().to_string();

        let error = for_data_mode(&projection).mount(&projection).unwrap_err();

        assert!(error.to_string().contains("extra_args"));
        assert!(!mountpoint.exists());
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

    #[test]
    fn sshfs_unmount_plan_keeps_primary_and_adds_legacy_only_when_present() {
        let s = spec(DataMode::Sshfs, ssh());
        let primary = SshPlacement::sshfs_unmount_argv(&s.mountpoint);
        let without_legacy = sshfs_unmount_plan(&s, &[]);
        assert_eq!(without_legacy, vec![primary.clone()]);

        let legacy_paths = s.legacy_mountpoints();
        let with_legacy = sshfs_unmount_plan(&s, &legacy_paths);
        assert_eq!(with_legacy[0], primary);
        assert_eq!(with_legacy.len(), 2);
        assert_eq!(
            with_legacy[1],
            SshPlacement::sshfs_unmount_argv(&legacy_paths[0])
        );
    }

    #[test]
    fn sshfs_unmount_plan_includes_v1_for_nondefault_placement() {
        let mut placement = match ssh() {
            Placement::Ssh(placement) => placement,
            _ => unreachable!(),
        };
        placement.port = 2222;
        let s = spec(DataMode::Sshfs, Placement::Ssh(placement));
        let legacy_paths = s.legacy_mountpoints();
        let plan = sshfs_unmount_plan(&s, &legacy_paths);
        let default_placement = match ssh() {
            Placement::Ssh(placement) => placement,
            _ => unreachable!(),
        };
        let v1_mountpoint = thegn_core::projection::mountpoint(&default_placement, "/srv/work");
        assert_eq!(plan.len(), 3);
        assert_eq!(plan[0], SshPlacement::sshfs_unmount_argv(&s.mountpoint));
        assert_eq!(plan[2], SshPlacement::sshfs_unmount_argv(&v1_mountpoint));
    }

    #[test]
    fn sshfs_unmount_plan_omits_unavailable_legacy_path() {
        let s = spec(DataMode::Sshfs, Placement::Local);
        assert_eq!(
            sshfs_unmount_plan(&s, &[]),
            vec![SshPlacement::sshfs_unmount_argv(&s.mountpoint)]
        );
    }

    #[test]
    fn legacy_unmount_success_warns_when_primary_unmount_fails() {
        let s = spec(DataMode::Sshfs, ssh());
        let captured = CapturedWarnings::default();
        let result = tracing::subscriber::with_default(captured.clone(), || {
            finish_sshfs_unmount(
                &s,
                Err(anyhow::anyhow!("primary mount is busy")),
                true,
                false,
            )
        });

        assert!(result.is_ok());
        let records = captured.records();
        assert_eq!(records.len(), 1);
        assert!(
            records[0]
                .get("error")
                .is_some_and(|value| value.contains("busy"))
        );
        assert_eq!(records[0].get("mountpoint"), Some(&s.mountpoint));
    }

    #[test]
    fn v1_legacy_unmount_success_does_not_mask_primary_failure() {
        let mut placement = match ssh() {
            Placement::Ssh(placement) => placement,
            _ => unreachable!(),
        };
        placement.port = 2222;
        let s = spec(DataMode::Sshfs, Placement::Ssh(placement));
        let default_placement = match ssh() {
            Placement::Ssh(placement) => placement,
            _ => unreachable!(),
        };
        let v1_mountpoint = thegn_core::projection::mountpoint(&default_placement, "/srv/work");
        assert!(s.legacy_mountpoints().contains(&v1_mountpoint));
        let result = finish_sshfs_unmount(
            &s,
            Err(anyhow::anyhow!("primary mount is busy")),
            false,
            true,
        );

        assert!(result.is_err());
    }

    #[test]
    fn reaper_unmounts_and_removes_legacy_dirs_only() {
        let mounts = tempfile::tempdir().unwrap();
        let legacy = mounts.path().join("dev-box-srv-work");
        let current = mounts
            .path()
            .join("dev-box-srv-work--0123456789abcdef0123456789abcdef");
        std::fs::create_dir(&legacy).unwrap();
        std::fs::create_dir(&current).unwrap();
        let mut calls = Vec::new();
        let reaped = reap_legacy_mounts_with(mounts.path(), |argv| {
            calls.push(argv.to_vec());
            Ok(())
        });
        assert_eq!(reaped, 1);
        assert_eq!(
            calls,
            vec![SshPlacement::sshfs_unmount_argv(legacy.to_str().unwrap())]
        );
        assert!(!legacy.exists());
        assert!(current.exists());
    }

    #[test]
    fn reaper_does_not_follow_legacy_named_symlink() {
        let mounts = tempfile::tempdir().unwrap();
        let target = mounts
            .path()
            .join("target--0123456789abcdef0123456789abcdef");
        let link = mounts.path().join("legacy-name");
        std::fs::create_dir(&target).unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let mut calls = Vec::new();
        let reaped = reap_legacy_mounts_with(mounts.path(), |argv| {
            calls.push(argv.to_vec());
            Ok(())
        });
        assert_eq!(reaped, 0);
        assert!(calls.is_empty());
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(target.is_dir());
    }
}
