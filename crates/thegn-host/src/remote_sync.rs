//! Materialize a local git worktree onto a remote ssh host as a self-contained
//! git clone, so an OCI container on that host can bind-mount it.
//!
//! The default sandbox mounts are path-preserving (`-v <local>:<local>`) and the
//! worktree's `.git` points at the local object store — none of which exist on a
//! remote box. So for a remote-placement OCI env we build a self-contained
//! clone on the remote: a `git bundle` of `HEAD`'s history (committed objects
//! only — inherently excludes `target/` etc.), streamed over the placement's ssh
//! control transport (which carries the configured `ProxyCommand`), `git clone`d
//! there, then the working state (tracked diff + `.gitignore`-filtered untracked
//! files) applied on top. Existing remote destinations are retained for explicit
//! recovery. Completed destinations are refreshed from the current local state
//! once per process, and a path is returned only after every materialization
//! step succeeds.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use thegn_svc::host::OciRunner;

/// Container-internal mountpoint for a worktree materialized on a remote host.
/// The base sandbox image creates `/workspace`, so the container path is fixed
/// and decoupled from the (remote, `$HOME`-relative) host path.
const REMOTE_WORKTREE_DEST: &str = "/workspace";

/// Successful remote worktrees for this process. Per-key locks serialize the
/// first materialization so concurrent pane preparation shares its result.
#[derive(Default)]
struct SyncCache {
    synced: Mutex<HashMap<String, String>>,
    in_flight: Mutex<HashMap<String, Arc<Mutex<()>>>>,
}

static SYNCED: LazyLock<SyncCache> = LazyLock::new(SyncCache::default);

fn sync_once(
    cache: &SyncCache,
    key: &str,
    sync: impl FnOnce() -> Result<String, String>,
) -> Result<String, String> {
    if let Some(path) = cache
        .synced
        .lock()
        .map_err(|_| "remote worktree sync cache is poisoned".to_string())?
        .get(key)
        .cloned()
    {
        return Ok(path);
    }

    let key_lock = cache
        .in_flight
        .lock()
        .map_err(|_| "remote worktree sync lock map is poisoned".to_string())?
        .entry(key.to_string())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone();
    let _guard = key_lock
        .lock()
        .map_err(|_| "remote worktree sync lock is poisoned".to_string())?;

    if let Some(path) = cache
        .synced
        .lock()
        .map_err(|_| "remote worktree sync cache is poisoned".to_string())?
        .get(key)
        .cloned()
    {
        return Ok(path);
    }

    let path = sync()?;
    cache
        .synced
        .lock()
        .map_err(|_| "remote worktree sync cache is poisoned".to_string())?
        .insert(key.to_string(), path.clone());
    Ok(path)
}

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
        "set -e; d=\"$HOME/thegn-worktrees\"/{slug}; marker=\"$d.thegn-incomplete\"; \
         mkdir -p \"$(dirname \"$d\")\"; \
         set -C; \
         if [ ! -e \"$d\" ] && : > \"$marker\" 2>/dev/null; then \
              if mkdir \"$d\"; then printf 'NEW:%s' \"$d\"; \
              else printf '%s' 'remote destination exists and was retained' >&2; exit 1; fi; \
         elif [ -d \"$d\" ] && [ ! -L \"$d\" ] && [ ! -e \"$marker\" ] && \
              [ -e \"$d/.git\" ] && top=$(git -C \"$d\" rev-parse --show-cdup 2>/dev/null) && [ -z \"$top\" ] && \
              : > \"$marker\" 2>/dev/null; then \
              rm -f \"$d.bundle\"; \
              printf 'REUSED:%s' \"$d\"; \
         else printf '%s' 'remote destination exists but is incomplete or not a Git checkout; retained for reconciliation' >&2; exit 1; fi",
        slug = q(slug),
    )
}

fn should_apply_tracked_diff(worktree: &str) -> Result<bool, String> {
    // `git diff --quiet HEAD` returns 1 for changes and >1 for an error.
    #[expect(
        clippy::disallowed_methods,
        reason = "inspect the local worktree diff status"
    )]
    let status = thegn_core::util::git_cmd(std::path::Path::new(worktree))
        .args(["diff", "--quiet", "HEAD"])
        .status()
        .map_err(|e| format!("could not inspect local tracked changes: {e}"))?;
    match status.code() {
        Some(0) => Ok(false),
        Some(1) => Ok(true),
        _ => Err(format!("could not inspect local tracked changes: {status}")),
    }
}

fn restore_command(dir: &str, branch: &str, recursive_submodules: bool) -> String {
    let restore = if branch.is_empty() || branch == "HEAD" {
        String::new()
    } else {
        format!("git -C {d} checkout -qB {b}; ", d = q(dir), b = q(branch))
    };
    let init = if recursive_submodules {
        format!(
            "git -c submodule.recurse=false -C {d} submodule update --init; ",
            d = q(dir)
        )
    } else {
        String::new()
    };
    format!("set -e; {restore}{init}rm -f {d}.bundle", d = q(dir))
}

fn refresh_command(dir: &str, branch: &str, recursive_submodules: bool) -> String {
    let checkout = if branch.is_empty() || branch == "HEAD" {
        format!(
            "git -C {d} checkout -q -f --detach FETCH_HEAD; ",
            d = q(dir)
        )
    } else {
        format!(
            "git -C {d} checkout -q -f -B {b} FETCH_HEAD; ",
            d = q(dir),
            b = q(branch)
        )
    };
    let init = if recursive_submodules {
        format!(
            "git -c submodule.recurse=false -C {d} submodule update --init; ",
            d = q(dir)
        )
    } else {
        String::new()
    };
    format!(
        "set -e; git -C {d} fetch -q {bundle} HEAD; git -C {d} reset -q --hard; git -C {d} clean -ffdq; {checkout}{init}rm -f {bundle}",
        d = q(dir),
        bundle = q(&format!("{dir}.bundle")),
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
    match sync_worktree(
        &runner,
        worktree,
        &spec.name,
        recursive_submodules,
        warnings,
    ) {
        Ok(remote_path) => {
            thegn_core::host::mount_remote_worktree(spec, &remote_path, REMOTE_WORKTREE_DEST)
        }
        Err(e) => {
            let warning = format!(
                "remote worktree sync failed: {e}; after saving any work in it, manual recovery on the remote host: run `rm -rf \"$HOME/thegn-worktrees/{0}\" \"$HOME/thegn-worktrees/{0}.thegn-incomplete\" \"$HOME/thegn-worktrees/{0}.bundle\"`, then retry",
                spec.name
            );
            warnings.push(warning.clone());
            thegn_core::msg::warn(&format!("{warning} (worktree {worktree})"));
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
/// tree maps 1:1 to the container that binds it. A successful sync is cached
/// by worktree for this process; a later process refreshes the destination.
/// Existing destinations and failed attempts are retained.
pub(crate) fn sync_worktree(
    runner: &OciRunner,
    worktree: &str,
    slug: &str,
    recursive_submodules: bool,
    warnings: &mut Vec<String>,
) -> Result<String, String> {
    sync_once(&SYNCED, worktree, || {
        sync_worktree_uncached(runner, worktree, slug, recursive_submodules, warnings)
    })
}

fn sync_worktree_uncached(
    runner: &OciRunner,
    worktree: &str,
    slug: &str,
    recursive_submodules: bool,
    warnings: &mut Vec<String>,
) -> Result<String, String> {
    // 1. Claim the destination atomically. Never follow or remove an existing
    //    checkout, symlink, or interrupted attempt; those require reconciliation.
    let prep = claim_command(slug);
    let (ok, path, err) = runner.host_exec(&prep, secs(60))?;
    let claim = path.trim();
    if !ok || claim.is_empty() {
        return Err(format!(
            "remote destination is unavailable or already exists; retained for reconciliation: {}",
            err.trim()
        ));
    }
    let (reused, dir) = if let Some(path) = claim.strip_prefix("REUSED:") {
        (true, path.to_string())
    } else if let Some(path) = claim.strip_prefix("NEW:") {
        (false, path.to_string())
    } else {
        return Err("remote destination claim returned an invalid response".to_string());
    };
    if reused {
        warnings.push(format!(
            "reusing completed remote worktree at {dir} and syncing current local state"
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
    if !reused {
        let (ok, _o, err) = runner.host_exec(
            &format!("git clone -q {d}.bundle {d}", d = q(&dir)),
            secs(600),
        )?;
        if !ok {
            // Retain the bundle as part of this interrupted attempt; the incomplete
            // marker identifies the destination as requiring reconciliation.
            return Err(format!("remote clone failed: {}", err.trim()));
        }
    }
    // Restore the branch name (clone lands on detached HEAD), or advance an
    // existing checkout to the newly fetched local HEAD.
    let branch = local_git(worktree, &["rev-parse", "--abbrev-ref", "HEAD"])
        .ok_or_else(|| "could not determine local worktree branch".to_string())?;
    let (ok, _out, err) = runner.host_exec(
        &if reused {
            refresh_command(&dir, &branch, recursive_submodules)
        } else {
            restore_command(&dir, &branch, recursive_submodules)
        },
        secs(600),
    )?;
    if !ok {
        return Err(format!(
            "remote branch or submodule setup failed: {}",
            err.trim()
        ));
    }

    // 3. Apply working state: tracked diff, then untracked files.
    if should_apply_tracked_diff(worktree)? {
        runner.pipe_local_to_host(
            &[
                "git".into(),
                "-C".into(),
                worktree.into(),
                "diff".into(),
                "--binary".into(),
                "HEAD".into(),
            ],
            &format!(
                "cd {d} && git apply --whitespace=nowarn - 2>/dev/null",
                d = q(&dir)
            ),
            secs(180),
        )?;
    }
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
    runner
        .host_exec(
            &format!("rm -f {d}.thegn-incomplete", d = q(&dir)),
            secs(30),
        )
        .and_then(|(ok, _out, err)| {
            if ok {
                Ok(())
            } else {
                Err(format!(
                    "remote completion marker cleanup failed: {}",
                    err.trim()
                ))
            }
        })?;
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::{
        SyncCache, claim_command, refresh_command, restore_command, should_apply_tracked_diff,
        sync_once,
    };
    use std::process::Command;
    use std::sync::{
        Arc, Barrier,
        atomic::{AtomicUsize, Ordering},
    };

    fn temp_home(label: &str) -> std::path::PathBuf {
        let home = std::env::temp_dir().join(format!(
            "thegn-remote-sync-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&home).unwrap();
        home
    }

    #[expect(
        clippy::disallowed_methods,
        reason = "exercise the remote claim shell fixture"
    )]
    fn run_sh(home: &std::path::Path, command: &str) -> std::process::Output {
        Command::new("sh")
            .args(["-c", command])
            .envs(thegn_core::testenv::state_env_all(home))
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap()
    }

    #[expect(clippy::disallowed_methods, reason = "create a local Git fixture")]
    fn run_git(dir: &std::path::Path, args: &[&str]) {
        let output = thegn_core::util::git_cmd(dir)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[expect(clippy::disallowed_methods, reason = "inspect a local Git fixture")]
    fn run_git_output(dir: &std::path::Path, args: &[&str]) -> String {
        let output = thegn_core::util::git_cmd(dir)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    #[test]
    fn destination_claim_refuses_existing_checkout_without_removing_it() {
        let home = temp_home("claim");
        let command = claim_command("sandbox");
        let first = run_sh(&home, &command);
        assert!(first.status.success());
        let marker = home.join("thegn-worktrees/sandbox/user-data");
        std::fs::write(&marker, b"keep").unwrap();

        let second = run_sh(&home, &command);
        assert!(!second.status.success());
        assert_eq!(std::fs::read(&marker).unwrap(), b"keep");
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn restore_failures_stop_before_bundle_cleanup() {
        let home = temp_home("restore");
        let checkout_dir = home.join("checkout");
        let checkout_bundle = home.join("checkout.bundle");
        std::fs::write(&checkout_bundle, b"retain").unwrap();
        let checkout = restore_command(checkout_dir.to_str().unwrap(), "main", true);
        let checkout = format!("git() {{ return 41; }}; {checkout}");
        let checkout_result = run_sh(&home, &checkout);
        assert!(!checkout_result.status.success());
        assert!(checkout_bundle.exists());

        let submodule_dir = home.join("submodule");
        let submodule_bundle = home.join("submodule.bundle");
        std::fs::write(&submodule_bundle, b"retain").unwrap();
        let submodule = restore_command(submodule_dir.to_str().unwrap(), "", true);
        let submodule = format!("git() {{ return 41; }}; {submodule}");
        let submodule_result = run_sh(&home, &submodule);
        assert!(!submodule_result.status.success());
        assert!(submodule_bundle.exists());
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn destination_claim_reuses_completed_checkout_and_refuses_incomplete_one() {
        let home = temp_home("reconcile");
        let command = claim_command("complete");
        let first = run_sh(&home, &command);
        assert!(first.status.success());
        let complete = home.join("thegn-worktrees/complete");
        run_git(&complete, &["init", "-q"]);
        std::fs::remove_file(home.join("thegn-worktrees/complete.thegn-incomplete")).unwrap();
        std::fs::write(home.join("thegn-worktrees/complete.bundle"), b"stale").unwrap();
        let reuse = run_sh(&home, &command);
        assert!(reuse.status.success());
        assert!(!home.join("thegn-worktrees/complete.bundle").exists());
        assert_eq!(
            String::from_utf8_lossy(&reuse.stdout),
            format!("REUSED:{}", complete.display())
        );

        let incomplete_command = claim_command("incomplete");
        let incomplete_first = run_sh(&home, &incomplete_command);
        assert!(incomplete_first.status.success());
        let refuse = run_sh(&home, &incomplete_command);
        assert!(!refuse.status.success());
        assert!(
            home.join("thegn-worktrees/incomplete.thegn-incomplete")
                .exists()
        );
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn destination_claim_refuses_unmarked_directory_inside_home_repository() {
        let home = temp_home("nested-claim");
        run_git(&home, &["init", "-q"]);
        let destination = home.join("thegn-worktrees/sandbox");
        std::fs::create_dir_all(&destination).unwrap();

        let result = run_sh(&home, &claim_command("sandbox"));
        assert!(!result.status.success());
        assert!(!destination.join(".git").exists());
        assert!(
            !home
                .join("thegn-worktrees/sandbox.thegn-incomplete")
                .exists()
        );
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn reused_checkout_refreshes_to_new_local_commit() {
        let home = temp_home("refresh");
        let source = home.join("source");
        let checkout = home.join("checkout");
        std::fs::create_dir_all(&source).unwrap();
        run_git(&source, &["init", "-q"]);
        run_git(&source, &["config", "user.email", "thegn@example.invalid"]);
        run_git(&source, &["config", "user.name", "thegn test"]);
        let tracked = source.join("tracked.txt");
        std::fs::write(&tracked, "first\n").unwrap();
        run_git(&source, &["add", "tracked.txt"]);
        run_git(&source, &["commit", "-qm", "first"]);
        run_git(
            &source,
            &[
                "clone",
                "-q",
                source.to_str().unwrap(),
                checkout.to_str().unwrap(),
            ],
        );

        std::fs::write(&tracked, "second commit\n").unwrap();
        run_git(&source, &["add", "tracked.txt"]);
        run_git(&source, &["commit", "-qm", "second"]);
        let bundle = format!("{}.bundle", checkout.display());
        run_git(&source, &["bundle", "create", &bundle, "HEAD"]);
        let branch = run_git_output(&source, &["rev-parse", "--abbrev-ref", "HEAD"]);
        let command = refresh_command(checkout.to_str().unwrap(), &branch, false);
        let result = run_sh(&home, &command);
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(
            std::fs::read_to_string(checkout.join("tracked.txt")).unwrap(),
            "second commit\n"
        );
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    #[expect(clippy::disallowed_methods, reason = "test fixture")]
    fn reused_checkout_refresh_discards_prior_tracked_diff_before_reapplying_it() {
        let home = temp_home("refresh-dirty");
        let source = home.join("source");
        let checkout = home.join("checkout");
        std::fs::create_dir_all(&source).unwrap();
        run_git(&source, &["init", "-q"]);
        run_git(&source, &["config", "user.email", "thegn@example.invalid"]);
        run_git(&source, &["config", "user.name", "thegn test"]);
        let tracked = source.join("tracked.txt");
        std::fs::write(&tracked, "committed\n").unwrap();
        run_git(&source, &["add", "tracked.txt"]);
        run_git(&source, &["commit", "-qm", "initial"]);
        run_git(
            &source,
            &[
                "clone",
                "-q",
                source.to_str().unwrap(),
                checkout.to_str().unwrap(),
            ],
        );

        std::fs::write(&tracked, "dirty local state\n").unwrap();
        let patch = thegn_core::util::git_cmd(&source)
            .args(["diff", "--binary", "HEAD"])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(patch.status.success());
        let patch_path = home.join("tracked.patch");
        std::fs::write(&patch_path, patch.stdout).unwrap();
        run_git(&checkout, &["apply", patch_path.to_str().unwrap()]);

        let bundle = format!("{}.bundle", checkout.display());
        run_git(&source, &["bundle", "create", &bundle, "HEAD"]);
        let branch = run_git_output(&source, &["rev-parse", "--abbrev-ref", "HEAD"]);
        let result = run_sh(
            &home,
            &refresh_command(checkout.to_str().unwrap(), &branch, false),
        );
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        run_git(&checkout, &["apply", patch_path.to_str().unwrap()]);
        assert_eq!(
            std::fs::read_to_string(checkout.join("tracked.txt")).unwrap(),
            "dirty local state\n"
        );
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn tracked_diff_guard_skips_clean_worktree_and_selects_dirty_worktree() {
        let home = temp_home("diff-guard");
        run_git(&home, &["init", "-q"]);
        run_git(&home, &["config", "user.email", "thegn@example.invalid"]);
        run_git(&home, &["config", "user.name", "thegn test"]);
        let tracked = home.join("tracked.txt");
        std::fs::write(&tracked, "original\n").unwrap();
        run_git(&home, &["add", "tracked.txt"]);
        run_git(&home, &["commit", "-qm", "initial"]);
        assert!(!should_apply_tracked_diff(home.to_str().unwrap()).unwrap());
        std::fs::write(&tracked, "changed\n").unwrap();
        assert!(should_apply_tracked_diff(home.to_str().unwrap()).unwrap());
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn second_sync_for_worktree_uses_cached_path_without_refresh() {
        let cache = SyncCache::default();
        let calls = AtomicUsize::new(0);
        let first = sync_once(&cache, "/repo/worktree", || {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok("/remote/thegn-worktrees/sandbox".to_string())
        })
        .unwrap();
        let second = sync_once(&cache, "/repo/worktree", || {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok("unexpected refresh".to_string())
        })
        .unwrap();

        assert_eq!(first, "/remote/thegn-worktrees/sandbox");
        assert_eq!(second, first);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn concurrent_syncs_for_worktree_run_materialization_once() {
        let cache = Arc::new(SyncCache::default());
        let calls = Arc::new(AtomicUsize::new(0));
        let start = Arc::new(Barrier::new(3));
        let mut threads = Vec::new();
        for _ in 0..2 {
            let cache = Arc::clone(&cache);
            let calls = Arc::clone(&calls);
            let start = Arc::clone(&start);
            threads.push(std::thread::spawn(move || {
                start.wait();
                sync_once(&cache, "/repo/worktree", || {
                    calls.fetch_add(1, Ordering::SeqCst);
                    std::thread::sleep(std::time::Duration::from_millis(50));
                    Ok("/remote/thegn-worktrees/sandbox".to_string())
                })
                .unwrap()
            }));
        }
        start.wait();
        let paths = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect::<Vec<_>>();

        assert_eq!(paths[0], "/remote/thegn-worktrees/sandbox");
        assert_eq!(paths[1], paths[0]);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
