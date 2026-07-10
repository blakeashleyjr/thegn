//! Materialize a local git worktree onto a remote ssh host as a self-contained
//! git clone, so an OCI container on that host can bind-mount it.
//!
//! The default sandbox mounts are path-preserving (`-v <local>:<local>`) and the
//! worktree's `.git` points at the local object store — none of which exist on a
//! remote box. So for a remote-placement OCI env we build a **fresh clone** on
//! the remote instead: a `git bundle` of `HEAD`'s history (committed objects
//! only — inherently excludes `target/` etc.), streamed over the placement's ssh
//! control transport (which carries the configured `ProxyCommand`), `git clone`d
//! there, then the working state (tracked diff + `.gitignore`-filtered untracked
//! files) applied on top. Everything is best-effort; on success it returns the
//! absolute remote path the worktree was materialized at.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use superzej_svc::host::OciRunner;

/// Container-internal mountpoint for a worktree materialized on a remote host.
/// The base sandbox image creates `/workspace`, so the container path is fixed
/// and decoupled from the (remote, `$HOME`-relative) host path.
const REMOTE_WORKTREE_DEST: &str = "/workspace";

/// Worktrees already materialized on their remote this session → resolved remote
/// path. `prepare_sandbox_env` runs more than once per open (host provisioning
/// then the pane build); this makes the second call a cheap no-op that still
/// yields the path to retarget the mount. (v1: re-sync only on a fresh session;
/// push-back / incremental re-sync is a follow-up.)
static SYNCED: LazyLock<Mutex<HashMap<String, String>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Guard a REMOTE placement that resolved to a bare `Backend::None`. A remote
/// SSH placement has no container to hold the synced worktree, so degrading to
/// `none` would ship a `cd <local-worktree>` to a bare remote shell (the
/// "cd: can't cd to …" fallback). Bail with a [`SandboxHalt`] — unless the user
/// asked for `none` explicitly, or opted into `failover`. K8s/Provider `none` is
/// legitimate (the placement itself is the boundary); an SSH env with a
/// projection runs locally (`Local`) and never reaches here.
///
/// [`SandboxHalt`]: crate::agent::SandboxHalt
pub(crate) fn ssh_none_guard(
    spec: &superzej_core::sandbox::SandboxSpec,
    configured: superzej_core::config::SandboxBackend,
    failover: bool,
    env_name: &str,
    placement_label: &str,
) -> anyhow::Result<()> {
    let ssh = matches!(spec.placement, superzej_core::placement::Placement::Ssh(_));
    let explicit_none = configured == superzej_core::config::SandboxBackend::None;
    if ssh && !explicit_none && !failover {
        return Err(crate::agent::SandboxHalt {
            env_name: env_name.to_string(),
            placement: placement_label.to_string(),
            reason: "no container runtime found on the remote host (podman not \
                     detected over ssh); install a container runtime there, or \
                     set a reachable [sandbox] backend"
                .to_string(),
        }
        .into());
    }
    Ok(())
}

/// Halt reason when a non-local env's failover-off bring-up produced no runnable
/// sandbox: distinguish an *unreachable* host (ssh transport failed) from a
/// reachable host that simply has no container runtime, so the message never
/// reads "podman missing" when we never actually reached the box.
pub(crate) fn no_backend_reason(reachable: bool, warnings: &[String]) -> String {
    if !reachable {
        "couldn't reach the host to detect a container runtime (ssh transport \
         failed) — check connectivity, then retry"
            .to_string()
    } else if warnings.is_empty() {
        "no usable backend produced a runnable sandbox".to_string()
    } else {
        warnings.join("; ")
    }
}

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

fn q(s: &str) -> String {
    superzej_core::util::sh_quote(s)
}

/// Run a local `git -C <worktree> <args>` and return trimmed stdout on success.
/// Goes through `util::git_cmd` for the GIT_* env scrub (never a raw git spawn).
fn local_git(worktree: &str, args: &[&str]) -> Option<String> {
    // off-loop: the sandbox-prepare path runs on spawn_blocking, never the loop.
    #[expect(clippy::disallowed_methods)]
    let out = superzej_core::util::git_cmd(std::path::Path::new(worktree))
        .args(args)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// If `spec` is a REMOTE-placement OCI spec, materialize its worktree on the
/// remote host and repoint the container bind at it (`/workspace`). The default
/// path-preserving worktree + git-common + host-toolchain binds are all LOCAL
/// paths that don't exist on the remote; this replaces them with a self-contained
/// remote clone. Best-effort — a sync failure warns but still lets the container
/// come up (empty tree) rather than silently dropping to a local host shell.
/// No-op for local placements / non-OCI backends.
pub(crate) fn retarget_if_remote_oci(
    spec: &mut superzej_core::sandbox::SandboxSpec,
    worktree: &str,
    warnings: &mut Vec<String>,
) {
    if !spec.backend.is_oci() || spec.placement.is_local() {
        return;
    }
    let runner = OciRunner::new(spec.placement.clone());
    match sync_worktree(&runner, worktree, &spec.name) {
        Ok(remote_path) => {
            superzej_core::host::mount_remote_worktree(spec, &remote_path, REMOTE_WORKTREE_DEST)
        }
        Err(e) => {
            warnings.push(format!("remote worktree sync failed: {e}"));
            superzej_core::msg::warn(&format!("remote worktree sync failed for {worktree}: {e}"));
        }
    }
}

/// Sync `worktree` to `runner`'s remote host under `~/superzej-worktrees/<slug>`
/// and return the resolved absolute remote path. `slug` should be the sandbox
/// container name (deterministic + unique per worktree), so the materialized
/// tree maps 1:1 to the container that binds it.
pub(crate) fn sync_worktree(
    runner: &OciRunner,
    worktree: &str,
    slug: &str,
) -> Result<String, String> {
    if let Some(p) = SYNCED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(worktree)
    {
        return Ok(p.clone());
    }

    // 1. Prepare the remote dir. `$HOME` expands remotely; a prior clone is wiped
    //    (v1 re-clones each open). Echo the resolved absolute path for the mount.
    let prep = format!(
        "d=\"$HOME/superzej-worktrees/{slug}\"; rm -rf \"$d\" \"$d.bundle\"; \
         mkdir -p \"$(dirname \"$d\")\"; printf %s \"$d\"",
        slug = slug,
    );
    let (ok, path, err) = runner.host_exec(&prep, secs(60))?;
    let dir = path.trim().to_string();
    if !ok || dir.is_empty() {
        return Err(format!("remote mkdir failed: {}", err.trim()));
    }

    // 2. Stream a bundle of HEAD's history, then clone it on the remote.
    runner.pipe_local_to_host(
        &[
            "git".into(),
            "-C".into(),
            worktree.into(),
            "bundle".into(),
            "create".into(),
            "-".into(),
            "HEAD".into(),
        ],
        &format!("cat > {}.bundle", q(&dir)),
        secs(600),
    )?;
    let (ok, _o, err) = runner.host_exec(
        &format!("git clone -q {d}.bundle {d}", d = q(&dir)),
        secs(600),
    )?;
    if !ok {
        let _ = runner.host_exec(&format!("rm -f {d}.bundle", d = q(&dir)), secs(30));
        return Err(format!("remote clone failed: {}", err.trim()));
    }
    // Restore the branch name (clone lands on detached HEAD) + drop the bundle;
    // both best-effort now that the checkout exists.
    let branch = local_git(worktree, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_default();
    let restore = if branch.is_empty() || branch == "HEAD" {
        String::new()
    } else {
        format!(
            "git -C {d} checkout -qB {b} 2>/dev/null; ",
            d = q(&dir),
            b = q(&branch)
        )
    };
    let _ = runner.host_exec(&format!("{restore}rm -f {d}.bundle", d = q(&dir)), secs(60));

    // 3. Apply working state (best-effort): tracked diff, then untracked files.
    let _ = runner.pipe_local_to_host(
        &[
            "git".into(),
            "-C".into(),
            worktree.into(),
            "diff".into(),
            "HEAD".into(),
        ],
        &format!(
            "cd {d} && git apply --whitespace=nowarn - 2>/dev/null || true",
            d = q(&dir)
        ),
        secs(180),
    );
    let _ = runner.pipe_local_to_host(
        &[
            "sh".into(),
            "-c".into(),
            format!(
                "git -C {wt} ls-files --others --exclude-standard -z | tar -C {wt} --null -T - -cf -",
                wt = q(worktree),
            ),
        ],
        &format!("tar -C {d} -xf - 2>/dev/null || true", d = q(&dir)),
        secs(180),
    );
    SYNCED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(worktree.to_string(), dir.clone());
    Ok(dir)
}
