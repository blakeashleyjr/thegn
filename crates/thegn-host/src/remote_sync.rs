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
//! files) applied on top. Existing remote destinations are retained for explicit
//! recovery, and a path is returned only after every materialization step
//! succeeds.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use thegn_svc::host::OciRunner;

/// Container-internal mountpoint for a worktree materialized on a remote host.
/// The base sandbox image creates `/workspace`, so the container path is fixed
/// and decoupled from the (remote, `$HOME`-relative) host path.
const REMOTE_WORKTREE_DEST: &str = "/workspace";

/// Worktrees already materialized on their remote this session → resolved remote
/// path. `prepare_sandbox_env` runs more than once per open (host provisioning
/// then the pane build); this makes the second call a cheap no-op that still
/// yields the path to retarget the mount. A cache miss never replaces a remote
/// destination; existing state must be reconciled outside this materializer.
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
    spec: &thegn_core::sandbox::SandboxSpec,
    configured: thegn_core::config::SandboxBackend,
    degrade_allowed: bool,
    ask: bool,
    env_name: &str,
    placement_label: &str,
) -> anyhow::Result<()> {
    let ssh = matches!(spec.placement, thegn_core::placement::Placement::Ssh(_));
    let explicit_none = configured == thegn_core::config::SandboxBackend::None;
    if ssh && !explicit_none && !degrade_allowed {
        return Err(crate::agent::SandboxHalt {
            env_name: env_name.to_string(),
            placement: placement_label.to_string(),
            reason: "no container runtime found on the remote host (podman not \
                     detected over ssh); install a container runtime there, or \
                     set a reachable [sandbox] backend"
                .to_string(),
            ask,
            dormant: None,
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
         failed, retried 3×) — check connectivity; thegn keeps retrying in \
         the background"
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
    thegn_core::util::sh_quote(s)
}

fn claim_command(slug: &str) -> String {
    format!(
        "set -e; d=\"$HOME/thegn-worktrees\"/{slug}; \
         mkdir -p \"$(dirname \"$d\")\"; mkdir \"$d\"; printf %s \"$d\"",
        slug = q(slug),
    )
}

/// Run a local `git -C <worktree> <args>` and return trimmed stdout on success.
/// Goes through `util::git_cmd` for the GIT_* env scrub (never a raw git spawn).
fn local_git(worktree: &str, args: &[&str]) -> Option<String> {
    // off-loop: the sandbox-prepare path runs on spawn_blocking, never the loop.
    #[expect(clippy::disallowed_methods)]
    let out = thegn_core::util::git_cmd(std::path::Path::new(worktree))
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
/// remote clone. A sync failure warns and retains the remote destination rather
/// than silently dropping to a local host shell.
/// No-op for local placements / non-OCI backends.
pub(crate) fn retarget_if_remote_oci(
    spec: &mut thegn_core::sandbox::SandboxSpec,
    worktree: &str,
    recursive_submodules: bool,
    warnings: &mut Vec<String>,
) {
    if !spec.backend.is_oci() || spec.placement.is_local() {
        return;
    }
    let runner = OciRunner::new(spec.placement.clone());
    match sync_worktree(&runner, worktree, &spec.name, recursive_submodules) {
        Ok(remote_path) => {
            thegn_core::host::mount_remote_worktree(spec, &remote_path, REMOTE_WORKTREE_DEST)
        }
        Err(e) => {
            warnings.push(format!("remote worktree sync failed: {e}"));
            thegn_core::msg::warn(&format!("remote worktree sync failed for {worktree}: {e}"));
        }
    }
}

/// The final adjustments to a resolved spec just before `sandbox::ensure` creates
/// the container: remote-OCI worktree materialization ([`retarget_if_remote_oci`])
/// and degrading an unavailable strong OCI runtime to the daemon default
/// ([`degrade_oci_runtime`]). Both are best-effort and only push human-visible
/// warnings; neither fails the pane.
pub(crate) fn finalize_spec_before_ensure(
    spec: &mut thegn_core::sandbox::SandboxSpec,
    worktree: &str,
    recursive_submodules: bool,
    warnings: &mut Vec<String>,
) {
    retarget_if_remote_oci(spec, worktree, recursive_submodules, warnings);
    if let Some(w) = degrade_oci_runtime(spec) {
        thegn_core::msg::warn(&format!("{w} for {worktree}"));
        warnings.push(w);
    }
}

/// If `spec` requests a strong OCI runtime (`runsc`/`krun`) that isn't usable on
/// THIS host — the runtime binary is absent, or (for libkrun) `/dev/kvm` is
/// missing — clear it so the container falls back to the daemon default runtime
/// instead of failing `create --runtime …`. Returns the user-facing reason when
/// it degraded, else `None`. The keep-or-degrade rule is the unit-tested pure
/// [`thegn_core::sandbox_runtime::decide`]; here we only probe the host.
pub(crate) fn degrade_oci_runtime(spec: &mut thegn_core::sandbox::SandboxSpec) -> Option<String> {
    use thegn_core::sandbox_runtime::{RuntimeDecision, decide, runtime_req};
    if !spec.backend.is_oci() {
        return None;
    }
    let requested = spec.oci_runtime.clone();
    let requested = requested.as_deref()?;
    let req = runtime_req(requested.trim())?;
    let binary_present = thegn_core::util::which_path(req.binary).is_some();
    let kvm_present = std::path::Path::new("/dev/kvm").exists();
    match decide(Some(requested), binary_present, kvm_present) {
        RuntimeDecision::Keep => None,
        RuntimeDecision::Degrade(reason) => {
            spec.oci_runtime = None;
            Some(reason)
        }
    }
}

/// Sync `worktree` to `runner`'s remote host under `~/thegn-worktrees/<slug>`
/// and return the resolved absolute remote path. `slug` should be the sandbox
/// container name (deterministic + unique per worktree), so the materialized
/// tree maps 1:1 to the container that binds it. Existing destinations and
/// failed attempts are retained.
pub(crate) fn sync_worktree(
    runner: &OciRunner,
    worktree: &str,
    slug: &str,
    recursive_submodules: bool,
) -> Result<String, String> {
    if let Some(p) = SYNCED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(worktree)
    {
        return Ok(p.clone());
    }

    // 1. Claim the destination atomically. Never follow or remove an existing
    //    checkout, symlink, or interrupted attempt; those require reconciliation.
    let prep = claim_command(slug);
    let (ok, path, err) = runner.host_exec(&prep, secs(60))?;
    let dir = path.trim().to_string();
    if !ok || dir.is_empty() {
        return Err(format!(
            "remote destination is unavailable or already exists; retained for reconciliation: {}",
            err.trim()
        ));
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
        &format!("set -C; cat > {}.bundle", q(&dir)),
        secs(600),
    )?;
    let (ok, _o, err) = runner.host_exec(
        &format!("git clone -q {d}.bundle {d}", d = q(&dir)),
        secs(600),
    )?;
    if !ok {
        return Err(format!("remote clone failed: {}", err.trim()));
    }
    // Restore the branch name (clone lands on detached HEAD).
    let branch = local_git(worktree, &["rev-parse", "--abbrev-ref", "HEAD"])
        .ok_or_else(|| "could not determine local worktree branch".to_string())?;
    let restore = if branch.is_empty() || branch == "HEAD" {
        String::new()
    } else {
        format!(
            "git -C {d} checkout -qB {b} 2>/dev/null && ",
            d = q(&dir),
            b = q(&branch)
        )
    };
    let init = if recursive_submodules {
        format!(
            "git -c submodule.recurse=false -C {d} submodule update --init; ",
            d = q(&dir)
        )
    } else {
        String::new()
    };
    let (ok, _out, err) = runner.host_exec(
        &format!("set -e; {restore}{init}rm -f {d}.bundle", d = q(&dir)),
        secs(600),
    )?;
    if !ok {
        return Err(format!(
            "remote branch or submodule setup failed: {}",
            err.trim()
        ));
    }

    // 3. Apply working state: tracked diff, then untracked files.
    runner.pipe_local_to_host(
        &[
            "git".into(),
            "-C".into(),
            worktree.into(),
            "diff".into(),
            "HEAD".into(),
        ],
        &format!(
            "cd {d} && git apply --whitespace=nowarn - 2>/dev/null",
            d = q(&dir)
        ),
        secs(180),
    )?;
    runner.pipe_local_to_host(
        &[
            "bash".into(),
            "-o".into(),
            "pipefail".into(),
            "-c".into(),
            format!(
                "git -C {wt} ls-files --others --exclude-standard -z | tar -C {wt} --null -T - -cf -",
                wt = q(worktree),
            ),
        ],
        &format!("tar -C {d} -xf -", d = q(&dir)),
        secs(180),
    )?;
    SYNCED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(worktree.to_string(), dir.clone());
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::claim_command;
    use std::process::Command;

    #[test]
    fn destination_claim_refuses_existing_checkout_without_removing_it() {
        let home = std::env::temp_dir().join(format!(
            "thegn-remote-sync-claim-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&home).unwrap();

        let command = claim_command("sandbox");
        let first = Command::new("sh")
            .args(["-c", command.as_str()])
            .env("HOME", &home)
            .output()
            .unwrap();
        assert!(first.status.success());
        let marker = home.join("thegn-worktrees/sandbox/user-data");
        std::fs::write(&marker, b"keep").unwrap();

        let second = Command::new("sh")
            .args(["-c", command.as_str()])
            .env("HOME", &home)
            .output()
            .unwrap();
        assert!(!second.status.success());
        assert_eq!(std::fs::read(&marker).unwrap(), b"keep");
        std::fs::remove_dir_all(home).unwrap();
    }
}
