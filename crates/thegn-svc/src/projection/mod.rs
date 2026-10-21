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

mod manifest;

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
    manifest::claim_or_verify(spec)?;
    let argv = sshfs_mount_argv_for(s, &spec.remote_dir, &spec.mountpoint);
    run_sshfs_mount(&argv, &spec.mountpoint)?;
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
    let Placement::Ssh(ssh) = &spec.placement else {
        bail!("sshfs projection requires an ssh placement");
    };
    manifest::verify(spec)?;
    let candidates = spec.legacy_mountpoints();
    let slug_legacy_path = thegn_core::projection::legacy_mountpoint(&ssh.host, &spec.remote_dir);
    let legacy_paths: Vec<_> = candidates
        .into_iter()
        .filter(|path| std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_dir()))
        .collect();
    let plan = sshfs_unmount_plan(spec, &legacy_paths);
    let primary_result = run_unmount_bounded(&plan[0]);

    let mut slug_legacy_succeeded = false;
    let mut v1_legacy_succeeded = false;
    for (legacy_path, legacy_argv) in legacy_paths.iter().zip(plan.iter().skip(1)) {
        match run_unmount_bounded(legacy_argv) {
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
    let mounted = match std::fs::read_to_string("/proc/self/mountinfo") {
        Ok(text) => Some(thegn_core::projection::sshfs_mountpoints_from_mountinfo(
            &text,
        )),
        Err(error) => {
            tracing::debug!(%error, "could not read process mountinfo for legacy projection reaper");
            None
        }
    };
    reap_legacy_mounts_with(mounts_dir, mounted.as_ref(), run_unmount_bounded)
}

fn reap_legacy_mounts_with(
    mounts_dir: &std::path::Path,
    mounted: Option<&std::collections::BTreeSet<String>>,
    mut run_unmount: impl FnMut(&[String]) -> Result<()>,
) -> usize {
    let (scan_dir, mounted) = match std::fs::canonicalize(mounts_dir) {
        Ok(canonical_dir) => (canonical_dir, mounted),
        Err(error) => {
            tracing::debug!(%error, path = %mounts_dir.display(), "could not resolve legacy projection mounts directory");
            (mounts_dir.to_path_buf(), None)
        }
    };
    let entries = match std::fs::read_dir(&scan_dir) {
        Ok(entries) => entries,
        Err(error) => {
            tracing::debug!(%error, path = %scan_dir.display(), "could not scan legacy projection mounts");
            return 0;
        }
    };
    let mut reaped = 0;
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                tracing::debug!(%error, path = %scan_dir.display(), "could not read projection mount entry");
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
        let path_string = path.to_string_lossy().into_owned();
        let is_mounted = mounted.map(|mounts| mounts.contains(&path_string));
        let (is_real_dir, is_empty) = if is_mounted == Some(true) {
            // lstat on a dead FUSE root can block; mountinfo is sufficient to
            // identify this path for a non-lazy unmount attempt.
            (false, false)
        } else {
            let is_real_dir = match std::fs::symlink_metadata(&path) {
                Ok(metadata) => metadata.is_dir(),
                Err(error) => {
                    tracing::debug!(%error, path = %path.display(), "could not inspect legacy projection mount");
                    continue;
                }
            };
            let is_empty = if is_real_dir {
                match std::fs::read_dir(&path) {
                    Ok(mut entries) => match entries.next() {
                        None => true,
                        Some(Ok(_)) => false,
                        Some(Err(error)) => {
                            tracing::debug!(%error, path = %path.display(), "could not inspect legacy projection contents");
                            false
                        }
                    },
                    Err(error) => {
                        tracing::debug!(%error, path = %path.display(), "could not inspect legacy projection contents");
                        false
                    }
                }
            } else {
                false
            };
            (is_real_dir, is_empty)
        };
        match thegn_core::projection::legacy_reap_action(is_mounted, is_real_dir, is_empty) {
            thegn_core::projection::LegacyReap::Skip => continue,
            thegn_core::projection::LegacyReap::RemoveOnly => match std::fs::remove_dir(&path) {
                Ok(()) => reaped += 1,
                Err(error) => {
                    tracing::debug!(%error, path = %path.display(), "could not remove empty legacy projection mountpoint")
                }
            },
            thegn_core::projection::LegacyReap::Unmount => {
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
        }
    }
    reaped
}

const UNMOUNT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

fn run_unmount_bounded(argv: &[String]) -> Result<()> {
    run_unmount_with_timeout(argv, UNMOUNT_TIMEOUT)
}

fn run_unmount_with_timeout(argv: &[String], timeout: std::time::Duration) -> Result<()> {
    run_bounded(argv, timeout)
}

fn run_bounded(argv: &[String], timeout: std::time::Duration) -> Result<()> {
    use std::sync::atomic::AtomicBool;

    if argv.is_empty() {
        bail!("empty projection argv");
    }
    let mut command = std::process::Command::new(&argv[0]);
    command.args(&argv[1..]);
    let cancelled = AtomicBool::new(false);
    let limits = thegn_proc::Limits {
        stderr: thegn_proc::Stream::Tail { cap: 4096 },
        ..thegn_proc::Limits::silent(timeout)
    };
    let run = thegn_proc::run(&mut command, &limits, &cancelled);
    if !run.success() {
        let reason = match (&run.end, &run.leader) {
            (thegn_proc::End::TimedOut, _) => {
                format!("timed out after {}s", timeout.as_secs_f64())
            }
            (thegn_proc::End::Spawn(error), _) => format!("could not run: {error}"),
            (thegn_proc::End::LeaderExited, thegn_proc::Leader::Reaped(status)) => {
                format!("exited with {status}")
            }
            _ => format!("end={:?}, leader={:?}", run.end, run.leader),
        };
        let stderr = String::from_utf8_lossy(&run.stderr.bytes);
        let stderr = stderr.trim();
        if stderr.is_empty() {
            bail!("{} failed: {reason}", argv[0]);
        }
        bail!("{} failed: {reason}: {stderr}", argv[0]);
    }
    Ok(())
}

/// Planner value for a slow ssh handshake plus FUSE setup, not a measured ceiling.
const SSHFS_MOUNT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

fn run_sshfs_mount(argv: &[String], mountpoint: &str) -> Result<()> {
    run_mount_bounded(argv, SSHFS_MOUNT_TIMEOUT, || {
        run_unmount_bounded(&SshPlacement::sshfs_unmount_argv(mountpoint))
    })
}

fn run_mount_bounded(
    argv: &[String],
    timeout: std::time::Duration,
    rollback: impl FnOnce() -> Result<()>,
) -> Result<()> {
    if argv.is_empty() {
        bail!("empty projection argv");
    }

    let stderr_file = tempfile::tempfile().map_err(|error| {
        anyhow::anyhow!("{} failed: could not capture stderr: {error}", argv[0])
    })?;
    let stderr_reader = stderr_file.try_clone().map_err(|error| {
        anyhow::anyhow!("{} failed: could not capture stderr: {error}", argv[0])
    })?;
    let started = std::time::Instant::now();
    let mut child = std::process::Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::from(stderr_file))
        .spawn()
        .map_err(|error| anyhow::anyhow!("{} failed: could not run: {error}", argv[0]))?;
    let deadline = started + timeout;
    let mut interval = thegn_proc::POLL_INTERVAL;

    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => {
                let message = nonzero_exit_message(&argv[0], status, stderr_tail(stderr_reader));
                bail!("{message}");
            }
            Ok(None) => {}
            Err(error) => {
                let kill_error = match child.kill() {
                    Ok(()) => None,
                    Err(kill_error) if kill_error.kind() == std::io::ErrorKind::InvalidInput => {
                        None
                    }
                    Err(kill_error) => Some(kill_error),
                };
                let wait_error = child.wait().err();
                let cleanup = kill_error
                    .map(|kill_error| format!("; could not kill child: {kill_error}"))
                    .unwrap_or_default();
                let cleanup = wait_error
                    .map(|wait_error| format!("{cleanup}; could not reap child: {wait_error}"))
                    .unwrap_or(cleanup);
                bail!("{} failed: could not wait: {error}{cleanup}", argv[0]);
            }
        }

        let now = std::time::Instant::now();
        let Some(remaining) = deadline.checked_duration_since(now) else {
            break;
        };
        if remaining.is_zero() {
            break;
        }
        std::thread::sleep(interval.min(remaining));
        interval = thegn_proc::next_backoff(interval, remaining);
    }

    let kill_error = match child.kill() {
        Ok(()) => None,
        Err(error) if error.kind() == std::io::ErrorKind::InvalidInput => None,
        Err(error) => Some(error),
    };
    let wait_result = child.wait();
    let mut message = format!(
        "{} failed: timed out after {}s",
        argv[0],
        timeout.as_secs_f64()
    );
    append_stderr_tail(&mut message, stderr_tail(stderr_reader));
    if let Some(error) = kill_error {
        message.push_str(&format!("; could not kill child: {error}"));
    }
    if let Err(error) = wait_result {
        message.push_str(&format!("; could not reap child: {error}"));
    }
    if let Err(error) = rollback() {
        if !rollback_means_not_mounted(&error) {
            message.push_str(&format!("; rollback unmount also failed: {error}"));
        }
    }
    Err(anyhow::anyhow!(message))
}

fn append_stderr_tail(message: &mut String, tail: Result<String>) {
    match tail {
        Ok(tail) if !tail.is_empty() => {
            message.push_str(": ");
            message.push_str(&tail);
        }
        Ok(_) => {}
        Err(error) => message.push_str(&format!("; could not read stderr tail: {error}")),
    }
}

fn nonzero_exit_message(
    argv0: &str,
    status: impl std::fmt::Display,
    tail: Result<String>,
) -> String {
    let mut message = format!("{argv0} failed: exited with {status}");
    append_stderr_tail(&mut message, tail);
    message
}

fn rollback_means_not_mounted(error: &anyhow::Error) -> bool {
    let message = format!("{error:#}").to_ascii_lowercase();
    message.contains("not mounted") || message.contains("not found in /etc/mtab")
}

fn stderr_tail(mut file: std::fs::File) -> Result<String> {
    use std::io::{Read, Seek, SeekFrom};

    let len = file.metadata()?.len().min(4096);
    file.seek(SeekFrom::End(-i64::try_from(len)?))?;
    let mut bytes = Vec::with_capacity(usize::try_from(len)?);
    file.read_to_end(&mut bytes)?;
    Ok(String::from_utf8_lossy(&bytes).trim().to_string())
}

/// Planner value for large pulls over slow links, not a measured ceiling.
const RSYNC_PULL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30 * 60);

/// Planner value for large pushes over slow links, not a measured ceiling.
const RSYNC_PUSH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30 * 60);

fn run_rsync_pull(argv: &[String]) -> Result<()> {
    run_bounded(argv, RSYNC_PULL_TIMEOUT)
}

fn run_rsync_push(argv: &[String]) -> Result<()> {
    run_bounded(argv, RSYNC_PUSH_TIMEOUT)
}

/// `rsync -e` transport flag carrying the placement's ssh knobs.
fn rsync_ssh_opt(s: &SshPlacement) -> Vec<String> {
    let mut ssh = vec!["ssh".to_string(), "-o".into(), "BatchMode=yes".into()];
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

/// Reference `sync` projection: stage the initial pull before replacing the
/// empty local working copy, so a failed `--delete` mirror cannot damage the
/// live tree. Later pulls sync in place to keep existing pane working
/// directories valid. A changed-files manifest is what `rsync` computes
/// internally; provider file-API adapters override this. One-direction-each
/// (pull on mount, push on refresh/close) — true bidirectional conflict
/// resolution is out of scope.
fn mount_sync(spec: &ProjectionSpec) -> Result<Mounted> {
    mount_sync_with(spec, run_rsync_pull)
}

fn mount_sync_with(
    spec: &ProjectionSpec,
    run: impl FnOnce(&[String]) -> Result<()>,
) -> Result<Mounted> {
    let Placement::Ssh(s) = &spec.placement else {
        bail!("sync projection requires an ssh placement");
    };
    let was_claimed = std::fs::symlink_metadata(manifest::manifest_path(spec)).is_ok();
    manifest::claim_or_verify(spec)?;

    let mountpoint = std::path::Path::new(&spec.mountpoint);
    let mut entries = std::fs::read_dir(mountpoint).map_err(|error| {
        anyhow::anyhow!(
            "could not inspect projection mountpoint {}: {error}",
            mountpoint.display()
        )
    })?;
    if was_claimed || entries.next().is_some() {
        let mut argv = vec!["rsync".into(), "-az".into(), "--delete".into()];
        argv.extend(rsync_ssh_opt(s));
        argv.push(rsync_remote(s, &spec.remote_dir));
        argv.push(local_dir(&spec.mountpoint));
        run(&argv)?;
        return Ok(Mounted {
            local_cwd: Some(spec.mountpoint.clone()),
        });
    }

    let mut nonce = [0_u8; 8];
    getrandom::fill(&mut nonce)
        .map_err(|error| anyhow::anyhow!("could not generate projection staging name: {error}"))?;
    let generation = nonce
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let parent = mountpoint.parent().ok_or_else(|| {
        anyhow::anyhow!(
            "mountpoint {} has no parent directory",
            mountpoint.display()
        )
    })?;
    let name = mountpoint
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("mountpoint {} has no file name", mountpoint.display()))?
        .to_string_lossy();
    let staging = parent.join(format!("{name}.thegn-stage-{generation}"));
    let old = parent.join(format!("{name}.thegn-old-{generation}"));
    std::fs::create_dir(&staging).map_err(|error| {
        anyhow::anyhow!(
            "could not create fresh projection staging directory {}: {error}",
            staging.display()
        )
    })?;
    if let Err(error) = thegn_core::fsperm::ensure_private_dir(&staging) {
        cleanup_projection_dir(&staging, "staging directory after permission failure");
        return Err(anyhow::anyhow!(
            "could not secure projection staging directory {}: {error}",
            staging.display()
        ));
    }

    let mut argv = vec!["rsync".into(), "-az".into(), "--delete".into()];
    argv.extend(rsync_ssh_opt(s));
    argv.push(rsync_remote(s, &spec.remote_dir));
    argv.push(local_dir(&staging.to_string_lossy()));
    if let Err(error) = run(&argv) {
        cleanup_projection_dir(&staging, "staging directory after failed pull");
        return Err(error);
    }

    let staging_meta = std::fs::symlink_metadata(&staging).map_err(|error| {
        anyhow::anyhow!(
            "could not verify projection staging directory {}: {error}",
            staging.display()
        )
    });
    match staging_meta {
        Ok(metadata)
            if metadata.is_dir()
                && !metadata.file_type().is_symlink()
                && thegn_core::fsperm::owned_by_current_user(&metadata) => {}
        Ok(_) => {
            cleanup_projection_dir(&staging, "untrusted staging directory");
            bail!(
                "projection staging path {} is not a real directory owned by the current user",
                staging.display()
            );
        }
        Err(error) => {
            cleanup_projection_dir(&staging, "unverifiable staging directory");
            return Err(error);
        }
    }

    if let Err(error) = std::fs::rename(mountpoint, &old) {
        cleanup_projection_dir(&staging, "staging directory after failed swap");
        return Err(anyhow::anyhow!(
            "could not move current projection {} aside: {error}",
            mountpoint.display()
        ));
    }
    if let Err(error) = std::fs::rename(&staging, mountpoint) {
        let restore = std::fs::rename(&old, mountpoint);
        cleanup_projection_dir(&staging, "staging directory after failed promotion");
        return match restore {
            Ok(()) => Err(anyhow::anyhow!(
                "could not promote projection staging directory {}: {error}",
                staging.display()
            )),
            Err(restore_error) => Err(anyhow::anyhow!(
                "could not promote projection staging directory {}: {error}; could not restore previous projection {}: {restore_error}",
                staging.display(),
                mountpoint.display()
            )),
        };
    }
    cleanup_projection_dir(&old, "previous projection directory");

    Ok(Mounted {
        local_cwd: Some(spec.mountpoint.clone()),
    })
}

fn cleanup_projection_dir(path: &std::path::Path, description: &str) {
    if let Err(error) = std::fs::remove_dir_all(path) {
        tracing::warn!(path = %path.display(), %error, "could not remove {description}");
    }
}

/// Push the local working copy back to the remote (no `--delete`: never reaches
/// in and removes remote files the local copy lacks).
fn push_sync(spec: &ProjectionSpec) -> Result<()> {
    let Placement::Ssh(s) = &spec.placement else {
        bail!("sync projection requires an ssh placement");
    };
    manifest::verify(spec)?;
    let mut argv = vec!["rsync".into(), "-az".into()];
    argv.extend(rsync_ssh_opt(s));
    argv.push(local_dir(&spec.mountpoint));
    argv.push(rsync_remote(s, &spec.remote_dir));
    run_rsync_push(&argv)
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
        assert_eq!(
            rsync_ssh_opt(&p),
            vec!["-e", "ssh -o BatchMode=yes -p 2222 -i /k/id"]
        );
        assert_eq!(rsync_remote(&p, "/srv/work/"), "dev-box:/srv/work/");
        assert_eq!(local_dir("/tmp/m/"), "/tmp/m/");
        // Default port omits -p.
        let q = SshPlacement::plain("h".into(), 22, false, TransportKind::Ssh);
        assert_eq!(rsync_ssh_opt(&q), vec!["-e", "ssh -o BatchMode=yes"]);
    }

    #[test]
    fn rsync_ssh_opt_carries_config_and_jump_host() {
        let mut placement = SshPlacement::plain("dev-box".into(), 22, false, TransportKind::Ssh);
        placement.ssh_config = Some("/cfg".into());
        placement.jump_host = Some("bastion".into());

        assert_eq!(
            rsync_ssh_opt(&placement),
            vec!["-e", "ssh -o BatchMode=yes -F /cfg -J bastion"]
        );
    }

    #[test]
    fn rsync_ssh_opt_quotes_identity_and_appends_extra_args() {
        let mut placement = SshPlacement::plain("dev-box".into(), 22, false, TransportKind::Ssh);
        placement.identity = Some("/k/my id".into());
        placement.extra_args = vec!["-v".into()];

        assert_eq!(
            rsync_ssh_opt(&placement),
            vec!["-e", "ssh -o BatchMode=yes -i '/k/my id' -v"]
        );
    }

    #[test]
    fn rsync_ssh_opt_disables_interactive_prompts() {
        let placement = SshPlacement::plain("dev-box".into(), 22, false, TransportKind::Ssh);

        assert_eq!(
            rsync_ssh_opt(&placement),
            vec!["-e", "ssh -o BatchMode=yes"]
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
        let mounts = thegn_core::testenv::symlinked_tempdir();
        let mounts_path = mounts.real_path();
        let legacy = mounts_path.join("dev-box-srv-work");
        let current = mounts_path.join("dev-box-srv-work--0123456789abcdef0123456789abcdef");
        std::fs::create_dir(&legacy).unwrap();
        std::fs::create_dir(&current).unwrap();
        let mut calls = Vec::new();
        let reaped = reap_legacy_mounts_with(mounts.path(), None, |argv| {
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
        if let Err(error) = thegn_core::fsperm::create_symlink(&target, &link, true) {
            if cfg!(unix) {
                panic!("symlink fixture: {error}");
            }
            return;
        }
        let mut calls = Vec::new();
        let reaped = reap_legacy_mounts_with(mounts.path(), None, |argv| {
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

    #[test]
    fn reaper_removes_known_unmounted_empty_legacy_directory_without_unmount() {
        let mounts = tempfile::tempdir().unwrap();
        let legacy = mounts.path().join("dev-box-srv-work");
        std::fs::create_dir(&legacy).unwrap();
        let mut calls = Vec::new();

        let reaped = reap_legacy_mounts_with(
            mounts.path(),
            Some(&std::collections::BTreeSet::new()),
            |argv| {
                calls.push(argv.to_vec());
                Ok(())
            },
        );

        assert_eq!(reaped, 1);
        assert!(calls.is_empty());
        assert!(!legacy.exists());
    }

    #[test]
    fn reaper_leaves_known_unmounted_nonempty_legacy_directory_alone() {
        let mounts = tempfile::tempdir().unwrap();
        let legacy = mounts.path().join("dev-box-srv-work");
        std::fs::create_dir(&legacy).unwrap();
        std::fs::write(legacy.join("working-copy-file"), "data").unwrap();
        let mut calls = Vec::new();

        let reaped = reap_legacy_mounts_with(
            mounts.path(),
            Some(&std::collections::BTreeSet::new()),
            |argv| {
                calls.push(argv.to_vec());
                Ok(())
            },
        );

        assert_eq!(reaped, 0);
        assert!(calls.is_empty());
        assert!(legacy.join("working-copy-file").exists());
    }

    #[test]
    fn reaper_unmounts_a_mountinfo_legacy_path() {
        let mounts = thegn_core::testenv::symlinked_tempdir();
        let mounts_path = mounts.real_path();
        let legacy = mounts_path.join("dev-box-srv-work");
        std::fs::create_dir(&legacy).unwrap();
        let mounted = std::collections::BTreeSet::from([legacy.to_string_lossy().into_owned()]);
        let mut calls = Vec::new();

        let reaped = reap_legacy_mounts_with(mounts.path(), Some(&mounted), |argv| {
            calls.push(argv.to_vec());
            Ok(())
        });

        assert_eq!(reaped, 1);
        assert_eq!(
            calls,
            vec![SshPlacement::sshfs_unmount_argv(legacy.to_str().unwrap())]
        );
        assert!(!legacy.exists());
    }

    #[test]
    fn reaper_matches_mountinfo_path_through_symlinked_mounts_directory() {
        let mounts = thegn_core::testenv::symlinked_tempdir();
        let real_mounts = mounts.real_path();
        let legacy = real_mounts.join("dev-box-srv-work");
        std::fs::create_dir(&legacy).unwrap();
        let resolved_legacy = std::fs::canonicalize(&legacy).unwrap();
        let mounted =
            std::collections::BTreeSet::from([resolved_legacy.to_string_lossy().into_owned()]);
        let mut calls = Vec::new();

        let reaped = reap_legacy_mounts_with(mounts.path(), Some(&mounted), |argv| {
            calls.push(argv.to_vec());
            Ok(())
        });

        assert_eq!(reaped, 1);
        assert_eq!(
            calls,
            vec![SshPlacement::sshfs_unmount_argv(
                resolved_legacy.to_str().unwrap()
            )]
        );
        assert!(!legacy.exists());
    }

    #[test]
    fn bounded_unmount_runner_times_out_and_accepts_success() {
        let started = std::time::Instant::now();
        let timeout_result = run_unmount_with_timeout(
            &["sleep".into(), "5".into()],
            std::time::Duration::from_millis(200),
        );
        assert!(timeout_result.is_err());
        assert!(started.elapsed() < std::time::Duration::from_secs(3));

        assert!(
            run_unmount_with_timeout(&["true".into()], std::time::Duration::from_secs(2)).is_ok()
        );
    }

    #[test]
    fn staged_pull_never_deletes_into_the_working_copy() {
        let temp = tempfile::tempdir().unwrap();
        thegn_core::fsperm::ensure_private_dir(temp.path()).unwrap();
        let mut projection = spec(DataMode::Sync, ssh());
        projection.mountpoint = temp.path().join("mount").to_string_lossy().into_owned();
        let mut captured = Vec::new();

        mount_sync_with(&projection, |argv| {
            captured = argv.to_vec();
            Ok(())
        })
        .unwrap();

        assert!(captured.iter().any(|arg| arg == "--delete"));
        let destination = captured.last().unwrap();
        assert_ne!(destination, &local_dir(&projection.mountpoint));
        assert!(destination.ends_with('/'));
        let destination_path = std::path::Path::new(destination.trim_end_matches('/'));
        assert_eq!(
            destination_path.parent(),
            std::path::Path::new(&projection.mountpoint).parent()
        );
        assert!(
            destination_path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("mount.thegn-stage-")
        );
    }

    #[test]
    fn staged_pull_failure_leaves_the_working_copy_untouched() {
        let temp = tempfile::tempdir().unwrap();
        thegn_core::fsperm::ensure_private_dir(temp.path()).unwrap();
        let mut projection = spec(DataMode::Sync, ssh());
        projection.mountpoint = temp.path().join("mount").to_string_lossy().into_owned();

        let result = mount_sync_with(&projection, |argv| {
            let destination = std::path::Path::new(argv.last().unwrap().trim_end_matches('/'));
            std::fs::write(destination.join("partial.txt"), b"partial").unwrap();
            Err(anyhow::anyhow!("simulated rsync failure"))
        });

        assert!(result.is_err());
        let working_copy = std::path::Path::new(&projection.mountpoint);
        assert_eq!(std::fs::read_dir(working_copy).unwrap().count(), 0);
        assert_eq!(
            std::fs::read_dir(working_copy.parent().unwrap())
                .unwrap()
                .count(),
            2
        );
    }

    #[test]
    fn staged_pull_second_pull_keeps_mountpoint_inode() {
        let temp = tempfile::tempdir().unwrap();
        thegn_core::fsperm::ensure_private_dir(temp.path()).unwrap();
        let mut projection = spec(DataMode::Sync, ssh());
        projection.mountpoint = temp.path().join("mount").to_string_lossy().into_owned();

        mount_sync_with(&projection, |argv| {
            let destination = std::path::Path::new(argv.last().unwrap().trim_end_matches('/'));
            std::fs::write(destination.join("keep.txt"), b"keep").unwrap();
            Ok(())
        })
        .unwrap();
        let mountpoint = std::path::Path::new(&projection.mountpoint);
        let before = thegn_core::util::file_identity(mountpoint).unwrap();
        let mut second_destination = String::new();
        mount_sync_with(&projection, |argv| {
            second_destination = argv.last().unwrap().clone();
            let destination = std::path::Path::new(argv.last().unwrap().trim_end_matches('/'));
            std::fs::write(destination.join("new.txt"), b"new").unwrap();
            Ok(())
        })
        .unwrap();

        let working_copy = std::path::Path::new(&projection.mountpoint);
        assert_eq!(second_destination, local_dir(&projection.mountpoint));
        assert_eq!(
            thegn_core::util::file_identity(working_copy).unwrap(),
            before
        );
        assert_eq!(std::fs::read(working_copy.join("new.txt")).unwrap(), b"new");
        assert_eq!(
            std::fs::read(working_copy.join("keep.txt")).unwrap(),
            b"keep"
        );
        let siblings = std::fs::read_dir(working_copy.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(siblings.len(), 2);
        assert!(!siblings.iter().any(|name| name.contains(".thegn-stage-")));
        assert!(!siblings.iter().any(|name| name.contains(".thegn-old-")));
        assert!(manifest::verify(&projection).is_ok());
    }

    #[test]
    fn bounded_transport_returns_by_deadline_when_a_descendant_holds_the_pipe() {
        let started = std::time::Instant::now();
        let result = run_bounded(
            &["sh".into(), "-c".into(), "sleep 30 & sleep 30".into()],
            std::time::Duration::from_millis(300),
        );

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("timed out"));
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }

    #[test]
    fn bounded_transport_error_keeps_stderr_and_exit_status() {
        let result = run_bounded(
            &["sh".into(), "-c".into(), "echo boom >&2; exit 3".into()],
            std::time::Duration::from_secs(5),
        );

        let error = result.unwrap_err().to_string();
        assert!(error.contains("boom"));
        assert!(error.contains("exited with"));
    }

    #[test]
    fn bounded_transport_reports_a_missing_binary() {
        let result = run_bounded(
            &["thegn-no-such-binary-xyz".into()],
            std::time::Duration::from_secs(2),
        );

        assert!(result.unwrap_err().to_string().contains("could not run"));
    }

    #[test]
    fn bounded_transport_accepts_success() {
        assert!(run_bounded(&["true".into()], std::time::Duration::from_secs(2)).is_ok());
    }

    #[test]
    fn sshfs_mount_runner_times_out_reaps_and_rolls_back() {
        let rolled = std::cell::Cell::new(false);
        let started = std::time::Instant::now();
        let result = run_mount_bounded(
            &["sh".into(), "-c".into(), "sleep 30".into()],
            std::time::Duration::from_millis(300),
            || {
                rolled.set(true);
                Ok(())
            },
        );

        assert!(result.unwrap_err().to_string().contains("timed out"));
        assert!(rolled.get());
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }

    #[test]
    fn sshfs_mount_runner_rollback_failure_is_visible() {
        let result = run_mount_bounded(
            &["sh".into(), "-c".into(), "sleep 30".into()],
            std::time::Duration::from_millis(300),
            || Err(anyhow::anyhow!("fusermount boom")),
        );

        let error = result.unwrap_err().to_string();
        assert!(error.contains("timed out"));
        assert!(error.contains("fusermount boom"));
    }

    #[test]
    fn sshfs_mount_runner_omits_not_mounted_rollback_diagnostic() {
        let result = run_mount_bounded(
            &["sh".into(), "-c".into(), "sleep 30".into()],
            std::time::Duration::from_millis(300),
            || {
                Err(anyhow::anyhow!(
                    "fusermount3: entry for /mnt/test not found in /etc/mtab"
                ))
            },
        );

        let error = result.unwrap_err().to_string();
        assert!(error.contains("timed out"));
        assert!(!error.contains("rollback unmount also failed"));
    }

    #[test]
    fn sshfs_mount_runner_preserves_exit_status_when_stderr_tail_read_fails() {
        let message = nonzero_exit_message(
            "sshfs",
            "exit status: 3",
            Err(std::io::Error::other("tail unavailable").into()),
        );

        assert!(message.contains("exited with exit status: 3"));
        assert!(message.contains("could not read stderr tail: tail unavailable"));
    }

    #[test]
    fn sshfs_mount_runner_returns_when_the_leader_daemonizes() {
        let rolled = std::cell::Cell::new(false);
        let started = std::time::Instant::now();
        let result = run_mount_bounded(
            &["sh".into(), "-c".into(), "sleep 30 & exit 0".into()],
            std::time::Duration::from_secs(5),
            || {
                rolled.set(true);
                Ok(())
            },
        );

        assert!(result.is_ok());
        assert!(!rolled.get());
        assert!(started.elapsed() < std::time::Duration::from_secs(3));
    }

    #[test]
    fn sshfs_mount_runner_reports_nonzero_exit_with_stderr_tail() {
        let rolled = std::cell::Cell::new(false);
        let result = run_mount_bounded(
            &["sh".into(), "-c".into(), "echo boom >&2; exit 3".into()],
            std::time::Duration::from_secs(5),
            || {
                rolled.set(true);
                Ok(())
            },
        );

        let error = result.unwrap_err().to_string();
        assert!(error.contains("boom"));
        assert!(error.contains("exited with"));
        assert!(!rolled.get());
    }

    #[test]
    fn sshfs_mount_runner_caps_the_stderr_tail() {
        let result = run_mount_bounded(
            &[
                "sh".into(),
                "-c".into(),
                "yes x | head -c 100000 >&2; echo END >&2; exit 1".into(),
            ],
            std::time::Duration::from_secs(5),
            || Ok(()),
        );

        let error = result.unwrap_err().to_string();
        assert!(error.contains("END"));
        assert!(error.len() < 4096 + 200);
    }

    #[test]
    fn sshfs_mount_runner_reports_a_missing_binary() {
        let rolled = std::cell::Cell::new(false);
        let result = run_mount_bounded(
            &["thegn-no-such-binary-xyz".into()],
            std::time::Duration::from_secs(2),
            || {
                rolled.set(true);
                Ok(())
            },
        );

        assert!(result.unwrap_err().to_string().contains("could not run"));
        assert!(!rolled.get());
    }
}
