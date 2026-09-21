//! Reusable merge-queue primitives shared by every surface that enqueues or
//! clears the queue: the `thegn merge` CLI (`cmd/merge.rs`), the agent-facing
//! MCP `HouseMerge` tools (`mcp_merge.rs`), and the control-API daemon
//! (`daemon/service.rs`). Keeping the branch/target resolution and repo-scoped
//! clear in one place means the three surfaces behave identically.
//!
//! Lives in the host crate (not core) because repo-membership needs git
//! resolution (`integrate::main_checkout`), which is host-side.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use thegn_core::config::Config;
use thegn_core::db::{Db, MergeQueueRow};
use thegn_core::merge_lifecycle::LifecycleEvent;
use thegn_core::models::WorktreeRow;
use thegn_core::remote::GitLoc;
use thegn_core::store::{WorkspaceStore, WorktreeAuxStore};
use thegn_core::util;
use thegn_svc::git::{CliGit, GitBackend};

use crate::{integrate, merge_driver};

/// The branch a worktree is currently on (`None` when detached).
pub fn branch_of(worktree: &Path) -> Option<String> {
    util::git_out(worktree, &["symbolic-ref", "--quiet", "--short", "HEAD"])
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// The repo root (main checkout) a worktree belongs to.
pub fn repo_root_of(worktree: &Path) -> Option<PathBuf> {
    integrate::main_checkout(worktree)
}

/// The `GitLoc` of a repo root — the host where the target store (and so the
/// fold/gate/CAS) lives. `Local` for an on-host repo, ssh/provider from the
/// root's own `location`. The merge queue is anchored to this host: the drain
/// must run co-located with it (a remote target can't be folded in-process —
/// see `is_remote_target`).
pub fn target_loc(db: &Db, repo_root: &Path) -> Result<GitLoc> {
    let root_s = repo_root.to_string_lossy();
    let loc_str = db.location_for(&root_s)?;
    Ok(GitLoc::from_db(&root_s, loc_str.as_deref()))
}

/// A short human label for a target store's host (ssh host / provider prefix),
/// or `None` when it's local. For the "run the drain on that host" guidance.
pub fn target_host_label(loc: &GitLoc) -> Option<String> {
    match loc {
        GitLoc::Local(_) => None,
        GitLoc::Remote { ssh, .. } => Some(ssh.host.clone()),
        GitLoc::Provider { control_prefix, .. } => control_prefix.first().cloned(),
    }
}

/// Guard for the in-process drain/land/integrate paths: when the target repo
/// lives on another host, the fold/gate/CAS can't run here (the object store is
/// remote). Returns a ready-to-print message telling the user to run the drain
/// co-located with the target repo. Automatic remote/provider source fetching
/// is currently unsupported by canonical-history admission; source work must
/// be committed and integrated locally. `None` when the target is local.
///
/// (The convenience path — the local UI auto-dispatching to a merge-drain daemon
/// on the target host over ssh/iroh — needs remote-daemon reach that isn't wired
/// yet; see tasks.md J128/129. Target-host execution is necessary but does not
/// admit an off-host source.)
pub fn remote_target_guard(db: &Db, repo_root: &Path) -> Result<Option<String>> {
    let loc = target_loc(db, repo_root)?;
    let Some(host) = target_host_label(&loc) else {
        return Ok(None);
    };
    Ok(Some(format!(
        "This repo's target branch lives on another host ({host}). \
         The merge queue folds in the target's object store, so the drain must \
         run there. Commit and integrate source work locally on that host before \
         running `thegn merge drain`: automatic remote/provider source fetching \
         is currently unsupported by canonical-history admission."
    )))
}

/// Push the advanced target branch to `origin` — the `push` `remote_mode`'s
/// convergence step after a sprite drains its own clone. Surfaces git's stderr
/// on failure so a rejected push is a visible error, never a false success.
pub fn push_target(repo_root: &Path, target: &str) -> Result<()> {
    #[expect(clippy::disallowed_methods)] // one-shot CLI push, not a loop read
    let out = util::git_cmd(repo_root)
        .args(["push", "origin", target])
        .output()
        .with_context(|| format!("spawning `git push origin {target}`"))?;
    if !out.status.success() {
        anyhow::bail!(
            "`git push origin {target}` failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Queue rows belonging to a repo (membership rule shared with the in-app drain).
pub fn rows_for_repo(db: &Db, root: &Path) -> Vec<MergeQueueRow> {
    merge_driver::rows_for_repo(db, root)
}

/// Normalize a worktree path before it is used as a queue-row key.
///
/// The queue is keyed by worktree path, and the argument arrives straight from
/// the CLI — so `merge add .` used to store the literal `"."`. That is not just
/// ugly: the key is global across repos, so two repos would collide on it, and
/// the row-to-repo membership test re-resolves the key against the *current*
/// process cwd, which only matches from the directory it was added in. Git's own
/// `worktree list` reports absolute resolved paths, so matching that form keeps
/// the two consistent. Falls back to the input when the path can't be resolved
/// (it may simply not exist — the caller reports that).
pub fn canonical_worktree(worktree: &Path) -> PathBuf {
    std::fs::canonicalize(worktree).unwrap_or_else(|_| worktree.to_path_buf())
}

/// The repo root for a worktree, with an error that names the actual problem.
///
/// `main_checkout` runs `git -C <path> worktree list` and collapses every
/// failure to `None`, so a path that simply no longer exists (the common case
/// after `on_landed = "remove"` deleted it) reported "not inside a git
/// repository" — which sent the reader looking in entirely the wrong place.
fn resolve_repo_root(worktree: &Path) -> Result<PathBuf> {
    if !worktree.exists() {
        anyhow::bail!("no such worktree: {}", worktree.display());
    }
    integrate::main_checkout(worktree)
        .with_context(|| format!("{}: not inside a git repository", worktree.display()))
}

/// Enqueue a single worktree's current branch onto the merge queue, applying the
/// sidebar-folder lifecycle. Returns a short human message describing the
/// outcome (queued / skipped). Errors only on a genuinely broken worktree
/// (detached HEAD, not a repo) or a DB write failure.
pub fn enqueue_worktree(cfg: &Config, db: &Db, worktree: &Path) -> Result<String> {
    let worktree = &canonical_worktree(worktree);
    let root = resolve_repo_root(worktree)?;
    // Takes the whole `Config` rather than a `MergeQueueConfig`: only here is the
    // repo root known, and the per-repo layer can't be applied without it.
    let mq = &cfg.repo_merge_queue(&root);
    let target = integrate::resolve_target(mq, &root);
    let branch = branch_of(worktree)
        .with_context(|| format!("{}: not on a branch (detached HEAD?)", worktree.display()))?;
    let wt_s = worktree.to_string_lossy().to_string();
    if branch == target {
        return Ok(format!("skipped {branch} (that's the target branch)"));
    }
    db.enqueue_merge(&wt_s, &branch, &target)?;
    crate::merge_lifecycle::apply(mq, db, &root, &wt_s, &branch, LifecycleEvent::Enqueued);
    Ok(format!("queued {branch}"))
}

/// Immutable registry facts used to prepare a remote enqueue without holding
/// the daemon's SQLite mutex across a provider/SSH round trip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RegisteredRemoteWorktree {
    worktree: String,
    branch: String,
    repo_root: String,
    location: String,
}

/// A remote enqueue after its live branch and host-local target have been
/// resolved. Commit re-checks the registry facts, closing the race where a
/// provider is reprovisioned or rebound while the remote Git lookup runs.
#[derive(Debug, Clone)]
pub(crate) struct PreparedRemoteEnqueue {
    registered: RegisteredRemoteWorktree,
    target: String,
}

/// Read the exact host-canonical registry row for a control-plane merge add.
/// `Ok(None)` means this is a normal on-host worktree and should retain the
/// local enqueue path. Any non-empty but malformed/stale location is treated as
/// remote and fails later; it must never degrade into local filesystem access.
pub(crate) fn registered_remote_worktree(
    db: &Db,
    worktree: &str,
) -> Result<Option<RegisteredRemoteWorktree>> {
    let Some(row) = db
        .worktrees()?
        .into_iter()
        .find(|row| row.worktree == worktree)
    else {
        // The caller has not yet canonicalized a local path. An absent exact
        // key can therefore be a harmless symlink/trailing-slash alias; return
        // it to the local confinement path, which proves membership before any
        // Git access. Route credentials cannot exploit this because their
        // authorization binding requires the exact registered identifier.
        return Ok(None);
    };
    if row.location.trim().is_empty() || row.location.trim() == "local" {
        return Ok(None);
    }
    validate_remote_registry_row(row).map(Some)
}

fn validate_remote_registry_row(row: WorktreeRow) -> Result<RegisteredRemoteWorktree> {
    anyhow::ensure!(
        !row.repo_root.trim().is_empty(),
        "registered remote worktree {} has no target repository membership",
        row.worktree
    );
    anyhow::ensure!(
        !row.branch.trim().is_empty(),
        "registered remote worktree {} has no branch metadata",
        row.worktree
    );
    let loc = GitLoc::from_db(&row.worktree, Some(&row.location));
    anyhow::ensure!(
        loc.is_remote(),
        "registered remote worktree {} has an invalid or stale location descriptor",
        row.worktree
    );
    Ok(RegisteredRemoteWorktree {
        worktree: row.worktree,
        branch: row.branch,
        repo_root: row.repo_root,
        location: row.location,
    })
}

/// Resolve live remote Git state and the host-local target without touching the
/// caller-supplied host path. This is the only remote-I/O phase.
pub(crate) fn prepare_remote_enqueue(
    cfg: &Config,
    registered: RegisteredRemoteWorktree,
) -> Result<PreparedRemoteEnqueue> {
    let loc = GitLoc::from_db(&registered.worktree, Some(&registered.location));
    anyhow::ensure!(
        loc.is_remote(),
        "registered remote worktree {} has an invalid or stale location descriptor",
        registered.worktree
    );
    let branch = CliGit.current_branch(&loc).with_context(|| {
        format!(
            "remote branch lookup failed for registered worktree {}",
            registered.worktree
        )
    })?;
    anyhow::ensure!(
        !branch.is_empty() && branch != "HEAD",
        "remote branch lookup found a detached HEAD for registered worktree {}",
        registered.worktree
    );
    anyhow::ensure!(
        branch == registered.branch,
        "registered branch for {} is stale (registry {:?}, remote {:?}); reopen or reprovision the worktree before enqueueing",
        registered.worktree,
        registered.branch,
        branch
    );

    let repo_root = Path::new(&registered.repo_root);
    anyhow::ensure!(
        repo_root.is_dir(),
        "registered target repository is unavailable on this host: {}",
        registered.repo_root
    );
    // THE-515: the registry row is bookkeeping written by whoever registered
    // the worktree (THE-73) and may name a symlink alias; key the trusted
    // overlay by the resolved root like every git-derived caller does.
    let overlay_root = std::fs::canonicalize(repo_root).unwrap_or_else(|_| repo_root.to_path_buf());
    if let Some(refusal) = cfg.workspace_overlay_refusal(&overlay_root) {
        anyhow::bail!("{}: {refusal}", overlay_root.display());
    }
    let mq = cfg.repo_merge_queue(&overlay_root);
    let target = integrate::resolve_target(&mq, repo_root);
    Ok(PreparedRemoteEnqueue { registered, target })
}

/// Commit a prepared remote enqueue only if its authoritative registry facts
/// are unchanged. The correlated `enqueue_merge` write copies the registered
/// location into the host-owned queue row in the same SQLite statement.
pub(crate) fn commit_remote_enqueue(
    cfg: &Config,
    db: &Db,
    prepared: &PreparedRemoteEnqueue,
) -> Result<String> {
    let current = registered_remote_worktree(db, &prepared.registered.worktree)?
        .context("remote worktree location became local while enqueueing")?;
    anyhow::ensure!(
        current == prepared.registered,
        "registered remote worktree metadata changed while enqueueing; retry"
    );
    if current.branch == prepared.target {
        return Ok(format!(
            "skipped {} (that's the target branch)",
            current.branch
        ));
    }
    anyhow::ensure!(
        db.enqueue_merge_if_worktree_matches(
            &current.worktree,
            &current.branch,
            &current.repo_root,
            &current.location,
            &prepared.target,
        )?,
        "registered remote worktree metadata changed while enqueueing; retry"
    );
    let overlay_root = std::fs::canonicalize(&current.repo_root)
        .unwrap_or_else(|_| PathBuf::from(&current.repo_root));
    let mq = cfg.repo_merge_queue(&overlay_root);
    crate::merge_lifecycle::apply(
        &mq,
        db,
        Path::new(&current.repo_root),
        &current.worktree,
        &current.branch,
        LifecycleEvent::Enqueued,
    );
    Ok(format!("queued {}", current.branch))
}

/// Remove one worktree's branch from the queue AND un-file it from its
/// lifecycle folder — the symmetric teardown to [`enqueue_worktree`]. A plain
/// dequeue neither lands nor fails, so without the `Dequeued` lifecycle the
/// worktree would be stranded in the "Merging"/"Needs attention" folder its
/// enqueue filed it into (the sidebar/queue de-sync). The un-file is best-effort
/// and guarded host-side to lifecycle-managed folders; dropping the row is the
/// operation that can fail.
pub fn dequeue_worktree(cfg: &Config, db: &Db, worktree: &Path) -> Result<()> {
    let worktree = &canonical_worktree(worktree);
    let wt_s = worktree.to_string_lossy().to_string();
    db.remove_merge_entry(&wt_s)?;
    // `apply` only needs the worktree + repo root for a dequeue (the branch is
    // unused by the un-file), so an empty branch is fine; skip only if the repo
    // root can't be resolved (worktree dir already gone — nothing to un-file).
    if let Some(root) = integrate::main_checkout(worktree) {
        let mq = &cfg.repo_merge_queue(&root);
        crate::merge_lifecycle::apply(mq, db, &root, &wt_s, "", LifecycleEvent::Dequeued);
    }
    Ok(())
}

/// Drop every queue row for `root`'s repo, un-filing each from its lifecycle
/// folder. Returns the number removed.
pub fn clear_repo(cfg: &Config, db: &Db, root: &Path) -> Result<usize> {
    let rows = rows_for_repo(db, root);
    let n = rows.len();
    for r in &rows {
        dequeue_worktree(cfg, db, Path::new(&r.worktree))?;
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[expect(clippy::disallowed_methods)] // deterministic real-Git fixture, test only
    fn git(dir: &Path, args: &[&str]) {
        let output = util::git_cmd(dir).args(args).output().unwrap();
        assert!(
            output.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn init_repo(path: &Path) {
        std::fs::create_dir_all(path).unwrap();
        git(path, &["init", "-q", "-b", "main"]);
        git(path, &["config", "user.name", "Thegn Test"]);
        git(path, &["config", "user.email", "thegn@example.invalid"]);
        git(path, &["config", "commit.gpgsign", "false"]);
        std::fs::write(path.join("base.txt"), "base\n").unwrap();
        git(path, &["add", "-A"]);
        git(path, &["commit", "-q", "-m", "base"]);
    }

    #[test]
    fn route_to_host_enqueues_registered_metadata_but_holds_unverified_provider_source() {
        #[derive(Debug, PartialEq, Eq)]
        struct RepoSnapshot {
            refs: Vec<u8>,
            index: Vec<u8>,
            config: Vec<u8>,
            base: Vec<u8>,
            remote: Option<Vec<u8>>,
        }

        #[expect(clippy::disallowed_methods)] // private read-only Git snapshot
        fn snapshot(path: &Path) -> RepoSnapshot {
            let output = util::git_cmd(path).arg("show-ref").output().unwrap();
            assert!(output.status.success());
            let remote = match std::fs::read(path.join("remote.txt")) {
                Ok(bytes) => Some(bytes),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => panic!("private tracked file snapshot failed: {error}"),
            };
            RepoSnapshot {
                refs: output.stdout,
                index: std::fs::read(path.join(".git/index")).unwrap(),
                config: std::fs::read(path.join(".git/config")).unwrap(),
                base: std::fs::read(path.join("base.txt")).unwrap(),
                remote,
            }
        }

        let temp = tempfile::Builder::new()
            .prefix("thegn-remote-queue-history-")
            .tempdir_in(std::fs::canonicalize(std::env::temp_dir()).unwrap())
            .unwrap();
        let state = temp.path().join("state");
        let config_root = temp.path().join("config");
        let local = temp.path().join("local");
        let global = temp.path().join("gitconfig");
        let template = temp.path().join("template");
        std::fs::write(&global, "").unwrap();
        std::fs::create_dir(&template).unwrap();
        let _env = crate::testenv::EnvVarGuard::set(&[
            ("XDG_STATE_HOME", state.to_str().unwrap()),
            ("XDG_CONFIG_HOME", config_root.to_str().unwrap()),
            ("APPDATA", config_root.to_str().unwrap()),
            ("LOCALAPPDATA", local.to_str().unwrap()),
            ("THEGN_DIR", temp.path().to_str().unwrap()),
            ("THEGN_PROFILE", ""),
            ("GIT_CONFIG_GLOBAL", global.to_str().unwrap()),
            ("GIT_CONFIG_NOSYSTEM", "1"),
            ("GIT_CONFIG_COUNT", "0"),
            ("GIT_CONFIG_PARAMETERS", ""),
            ("GIT_TEMPLATE_DIR", template.to_str().unwrap()),
        ]);
        // CanonicalHistory opens its own registry: isolate and initialize it
        // before any fixture Git or provider work, not just the two queue DBs.
        let _private_registry = Db::open().unwrap();
        let host_repo = temp.path().join("host-repo");
        let remote_repo = temp.path().join("remote-repo");
        init_repo(&host_repo);
        git(
            temp.path(),
            &[
                "clone",
                "-q",
                &host_repo.to_string_lossy(),
                &remote_repo.to_string_lossy(),
            ],
        );
        git(&remote_repo, &["config", "user.name", "Thegn Test"]);
        git(
            &remote_repo,
            &["config", "user.email", "thegn@example.invalid"],
        );
        git(&remote_repo, &["config", "commit.gpgsign", "false"]);
        git(&remote_repo, &["checkout", "-q", "-b", "feat/remote"]);
        std::fs::write(remote_repo.join("remote.txt"), "remote\n").unwrap();
        git(&remote_repo, &["add", "-A"]);
        git(&remote_repo, &["commit", "-q", "-m", "remote"]);

        // This is the stable identifier the host registered and injected. It
        // intentionally does not exist on the host; only `location` reaches the
        // separate clone/object store.
        let host_canonical_id = temp.path().join("absent-host-path");
        assert!(!host_canonical_id.exists());
        let host_id = host_canonical_id.to_string_lossy().into_owned();
        let provider_marker = temp.path().join("provider-must-not-run");
        let provider_armed = temp.path().join("provider-armed");
        let provider_audit = temp.path().join("provider-lookups");
        assert_eq!(
            util::git_out(&remote_repo, &["rev-parse", "--abbrev-ref", "HEAD"])
                .unwrap()
                .trim(),
            "feat/remote"
        );
        let expected_query = util::sh_join(&[
            "git".into(),
            "-C".into(),
            remote_repo.to_str().unwrap().into(),
            "rev-parse".into(),
            "--abbrev-ref".into(),
            "HEAD".into(),
        ]);
        // Stub only the exact enqueue lookup; never evaluate a provider script
        // or source login profiles. The repositories and queue writes are real.
        let location = GitLoc::provider_db_string(
            &[
                "sh".into(),
                "-c".into(),
                concat!(
                    "if [ -e \"$1\" ]; then printf invoked > \"$2\"; exit 97; fi; ",
                    "if [ \"$#\" -ne 7 ] || [ \"$5\" != /bin/sh ] || ",
                    "[ \"$6\" != -lc ] || [ \"$7\" != \"$4\" ]; then ",
                    "printf unexpected > \"$2\"; exit 96; fi; ",
                    "printf 'lookup\\n' >> \"$3\"; printf 'feat/remote\\n'"
                )
                .into(),
                "private-provider-canary".into(),
                provider_armed.to_str().unwrap().into(),
                provider_marker.to_str().unwrap().into(),
                provider_audit.to_str().unwrap().into(),
                expected_query,
            ],
            &remote_repo.to_string_lossy(),
        );
        let db = Db::open_at(&temp.path().join("host.db")).unwrap();
        db.put_worktree(
            "repo/feat-remote",
            &host_repo.to_string_lossy(),
            &host_id,
            "feat/remote",
            Some(&location),
            None,
        )
        .unwrap();
        let remote_db = Db::open_at(&temp.path().join("remote.db")).unwrap();

        let mut cfg = Config::default();
        cfg.merge_queue.organize_folders = false;
        cfg.merge_queue.gate_command.clear();
        let registered = registered_remote_worktree(&db, &host_id)
            .unwrap()
            .expect("registered remote");
        let prepared = prepare_remote_enqueue(&cfg, registered).unwrap();
        assert_eq!(
            commit_remote_enqueue(&cfg, &db, &prepared).unwrap(),
            "queued feat/remote"
        );

        let rows = db.list_merge_queue().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].worktree, host_id);
        assert_eq!(rows[0].location, location);
        assert_eq!(rows[0].status, "queued");
        assert_eq!(rows[0].agent_attempts, 0);
        assert!(remote_db.list_merge_queue().unwrap().is_empty());
        assert_eq!(std::fs::read(&provider_audit).unwrap(), b"lookup\n");
        assert!(!provider_marker.exists());
        std::fs::write(&provider_armed, "drain must not invoke provider").unwrap();
        let host_before = snapshot(&host_repo);
        let source_before = snapshot(&remote_repo);
        let mut progress = Vec::new();

        let item = crate::merge_driver::QueueItem {
            worktree: rows[0].worktree.clone(),
            branch: rows[0].branch.clone(),
            location: rows[0].location.clone(),
            agent_attempts: rows[0].agent_attempts,
        };
        let outcome = crate::merge_driver::drive_queue(
            &cfg.merge_queue,
            &cfg,
            &host_repo,
            &db,
            vec![item],
            |step| progress.push(step.status.to_owned()),
        );
        assert_eq!(outcome.gate_error, ["feat/remote"]);
        assert!(outcome.landed.is_empty());
        assert!(outcome.ready.is_empty());
        assert!(outcome.deferred.is_empty());
        assert!(outcome.needs_human.is_empty());
        assert!(outcome.resyncs.is_empty());
        assert!(outcome.warnings.is_empty());
        assert_eq!(progress, ["folding", "gate_error"]);
        let after_rows = db.list_merge_queue().unwrap();
        assert_eq!(after_rows.len(), 1);
        let held = &after_rows[0];
        assert_eq!(held.status, "gate_error");
        assert_eq!(held.worktree, rows[0].worktree);
        assert_eq!(held.branch, rows[0].branch);
        assert_eq!(held.target_branch, rows[0].target_branch);
        assert_eq!(held.location, rows[0].location);
        assert_eq!(held.queued_at, rows[0].queued_at);
        assert_eq!(held.agent_attempts, rows[0].agent_attempts);
        assert!(held.result_oid.is_none());
        assert!(held.conflict_paths.is_none());
        let detail = held.error_detail.as_deref().unwrap();
        if thegn_core::sandbox_backend::host_os() == thegn_core::sandbox_backend::HostOs::Windows {
            assert!(detail.contains("verified local gate state is unsupported on this platform"));
            assert!(!detail.contains("unsupported for remote/provider"));
        } else {
            assert!(
                detail.contains(
                    "canonical history is unsupported for remote/provider merge operations"
                )
            );
        }
        assert!(!provider_marker.exists());
        assert_eq!(std::fs::read(&provider_audit).unwrap(), b"lookup\n");
        assert_eq!(snapshot(&host_repo), host_before);
        assert_eq!(snapshot(&remote_repo), source_before);
        assert!(remote_db.list_merge_queue().unwrap().is_empty());
    }

    #[test]
    fn stale_or_malformed_remote_metadata_never_falls_back_to_local_path() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        init_repo(&repo);
        let remote = temp.path().join("remote");
        git(
            temp.path(),
            &[
                "clone",
                "-q",
                &repo.to_string_lossy(),
                &remote.to_string_lossy(),
            ],
        );
        git(&remote, &["checkout", "-q", "-b", "feat"]);
        let id = temp.path().join("absent-id").to_string_lossy().into_owned();
        let db = Db::open_at(&temp.path().join("db.sqlite")).unwrap();
        db.put_worktree(
            "repo/feat",
            &repo.to_string_lossy(),
            &id,
            "feat",
            Some("not-a-location"),
            None,
        )
        .unwrap();
        let error = registered_remote_worktree(&db, &id)
            .unwrap_err()
            .to_string();
        assert!(error.contains("invalid or stale location"));
        assert!(db.list_merge_queue().unwrap().is_empty());

        let location = GitLoc::provider_db_string(&["env".into()], &remote.to_string_lossy());
        db.set_worktree_location(&id, &location).unwrap();
        let registered = registered_remote_worktree(&db, &id).unwrap().unwrap();
        let prepared = prepare_remote_enqueue(&Config::default(), registered).unwrap();
        db.put_worktree(
            "repo/feat",
            &repo.to_string_lossy(),
            &id,
            "changed",
            Some(&location),
            None,
        )
        .unwrap();
        let error = commit_remote_enqueue(&Config::default(), &db, &prepared)
            .unwrap_err()
            .to_string();
        assert!(error.contains("metadata changed"));
        assert!(db.list_merge_queue().unwrap().is_empty());
    }

    #[test]
    fn absent_exact_registry_key_delegates_to_local_alias_confinement() {
        let db = Db::open_memory().unwrap();
        assert_eq!(
            registered_remote_worktree(&db, "/local/alias").unwrap(),
            None
        );
    }
}
