//! Interactive rebase, driven without an editor: we generate the TODO
//! ourselves, write it to a scratch file inside the repo's private gitdir,
//! and hand git `GIT_SEQUENCE_EDITOR='cp <scratch>'` — git invokes the
//! editor as `$GIT_SEQUENCE_EDITOR <todo-path>`, so the cp overwrites the
//! todo with our prepared content. Content never crosses a shell-quoting
//! boundary (it travels via `write_git_path`), and the same mechanism works
//! locally, in tests, and over ssh.
//!
//! Once-off actions (squash/fixup/drop/reword/edit/move on a single commit,
//! amend-old-commit) compile into a generated todo over `<target>^..HEAD`.
//! While a rebase is paused (conflict/edit/break) the live todo at
//! `rebase-merge/git-rebase-todo` can be re-read and rewritten while holding
//! thegn's bounded advisory git-mutation lock. That excludes other thegn
//! writers only: git takes no lock on the todo, so an external editor or plain
//! `git rebase --edit-todo` can still race between the check and rename.

use super::{GitBackend, gpg_args, run_status, run_w};
use anyhow::{Context, Result, anyhow, bail};
use std::time::Duration;
use thegn_core::git_operand as op;
use thegn_core::rebase_todo::{
    TodoAction, TodoEntry, parse_todo, place_fixup, retag, serialize_todo, todo_from_log,
};
use thegn_core::remote::GitLoc;
use thegn_core::util::sh_quote;

/// Per-process sequence for unique scratch names; the PID separates concurrent
/// Thegn processes and the counter separates concurrent operations in one.
static TODO_SCRATCH_SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// Read sequencer markers without conflating read failures with absence.
fn read_rebase_marker(loc: &GitLoc, rel: &str) -> Result<Option<Vec<u8>>> {
    loc.read_git_path_checked(rel)
        .with_context(|| format!("read sequencer marker {rel}"))
}

/// Resolve and read the local live todo without following a final-component
/// symlink. A missing path is distinct from an unreadable or non-regular one.
fn read_local_pending_todo(loc: &GitLoc) -> Result<Option<(std::path::PathBuf, Vec<u8>)>> {
    let GitLoc::Local(worktree) = loc else {
        bail!("local todo read requested for a non-local worktree");
    };
    let resolved = loc
        .git_out(&["rev-parse", "--git-path", "rebase-merge/git-rebase-todo"])
        .ok_or_else(|| anyhow!("cannot resolve live rebase todo path"))?;
    let path = if resolved.starts_with('/') {
        std::path::PathBuf::from(resolved)
    } else {
        worktree.join(resolved)
    };
    let bytes = read_local_pending_todo_for_path(&path)?;
    Ok(bytes.map(|bytes| (path, bytes)))
}

/// Atomically publish a local todo only while its exact previously-read bytes
/// are still present. The caller holds thegn's bounded advisory mutation lock,
/// excluding other thegn writers only; git takes no lock on this file, so an
/// external editor can still race between the check and rename.
fn replace_local_pending_todo(path: &std::path::Path, expected: &[u8], bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("live rebase todo has no parent directory"))?;
    let sequence = TODO_SCRATCH_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let temporary = parent.join(format!(
        ".git-rebase-todo.thegn-{}-{sequence}",
        std::process::id()
    ));
    thegn_core::fsperm::write_owner_only_new(&temporary, bytes)
        .context("stage owner-only live rebase todo")?;
    let publish = (|| {
        let Some(current) = read_local_pending_todo_for_path(path)? else {
            bail!("rebase todo changed on disk — reload the editor before rewriting");
        };
        if current != expected {
            bail!("rebase todo changed on disk — reload the editor before rewriting");
        }
        thegn_core::fsperm::rename_replace(&temporary, path)
            .context("atomically replace live rebase todo")?;
        Ok(())
    })();
    if let Err(error) = publish {
        if let Err(cleanup_error) = std::fs::remove_file(&temporary)
            && cleanup_error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::debug!(target: "thegn::git", %cleanup_error, "could not remove staged live rebase todo");
        }
        return Err(error);
    }
    Ok(())
}

/// Re-read an already-resolved local todo path without following links.
fn read_local_pending_todo_for_path(path: &std::path::Path) -> Result<Option<Vec<u8>>> {
    let mut file = match thegn_core::fsperm::open_regular_file_nofollow(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            match std::fs::symlink_metadata(path) {
                Ok(_) => {
                    return Err(error).context("open live rebase todo without following links");
                }
                Err(metadata_error) if metadata_error.kind() == std::io::ErrorKind::NotFound => {
                    return Ok(None);
                }
                Err(metadata_error) => {
                    return Err(metadata_error).context("inspect live rebase todo");
                }
            }
        }
        Err(error) => return Err(error).context("open live rebase todo without following links"),
    };
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(&mut file, &mut bytes).context("read live rebase todo")?;
    Ok(Some(bytes))
}

/// Run a read-only git probe without taking the mutation lock or advancing the
/// write epoch. A non-zero exit is an error for these observations.
fn run_read(loc: &GitLoc, args: &[&str]) -> Result<String> {
    let (status, stdout) = run_status(loc, args)?;
    if status != 0 {
        bail!("git {} failed with exit status {status}", args.join(" "));
    }
    Ok(stdout)
}

#[derive(Debug, Clone, Default)]
pub struct RebaseOpts {
    /// Disable gpg signing for the rewrite (`[git] override_gpg`).
    pub override_gpg: bool,
}

/// How a rebase invocation ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RebaseOutcome {
    /// Ran to completion.
    Done,
    /// Stopped on conflicting paths — resolve, then continue/skip/abort.
    Conflict,
    /// Stopped deliberately (an `edit`/`break` todo entry).
    Paused,
}

/// Why a paused rebase is paused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PauseReason {
    Conflict,
    Edit,
}

/// The live state of an in-progress interactive rebase, read from the
/// `rebase-merge/` gitdir files.
#[derive(Debug, Clone)]
pub struct RebaseStatus {
    /// What we are rebasing onto (sha).
    pub onto: String,
    /// The branch being rebased (e.g. `refs/heads/feat` shortened).
    pub head_name: String,
    /// The entry git stopped at (best effort).
    pub stopped_sha: Option<String>,
    /// Entries already executed.
    pub done: Vec<TodoEntry>,
    /// Entries still pending (rewritable while paused).
    pub remaining: Vec<TodoEntry>,
    pub paused: PauseReason,
}

/// Run `git rebase -i` with a prepared todo, classifying the exit.
fn rebase_with_todo(
    loc: &GitLoc,
    base: &str,
    todo: &[TodoEntry],
    opts: &RebaseOpts,
) -> Result<RebaseOutcome> {
    with_rebase_txn(loc, &["rebase", "-i"], super::MUTATION_LOCK_WAIT, || {
        rebase_with_todo_inner(loc, base, todo, opts)
    })
}

fn with_rebase_txn<T>(
    loc: &GitLoc,
    args: &[&str],
    wait: Duration,
    f: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let _txn = if !loc.is_remote() && crate::bridge::for_loc(loc).is_none() {
        super::begin_mutation_txn(loc, args, wait)?
    } else {
        None
    };
    f()
}

fn rebase_with_todo_inner(
    loc: &GitLoc,
    base: &str,
    todo: &[TodoEntry],
    opts: &RebaseOpts,
) -> Result<RebaseOutcome> {
    if let Some(entry) = todo.iter().find(|entry| entry.action.is_commit())
        && matches!(
            &entry.action,
            TodoAction::Squash | TodoAction::Fixup | TodoAction::FixupC
        )
    {
        bail!(
            "cannot squash/fixup the first commit of the range: it has no previous commit — nothing was changed"
        );
    }
    // Guard the todo-build → rebase-apply window: `git rebase -i <base>` operates
    // on the CURRENT `base..HEAD`, but the todo was built from an earlier snapshot
    // (possibly under a separate lock). If a commit landed on the branch in
    // between, it is in `base..HEAD` yet absent from the todo, and git rebase
    // silently DROPS any in-range commit without a todo line — losing history.
    // Every planned commit (including `drop`ped ones, which keep a `drop <sha>`
    // line) is in the todo, so a range commit missing from it means HEAD advanced.
    // Bail loudly rather than rewrite the branch to a lossy result.
    {
        let range = if base == "--root" {
            "HEAD".to_string()
        } else {
            format!("{base}..HEAD")
        };
        let revs = run_read(loc, &["rev-list", "--reverse", &range])
            .context("verify rebase range before starting")?;
        let expected: Vec<String> = revs.split_whitespace().map(str::to_owned).collect();
        // Panel-built todos carry abbreviated ids. Compare canonical commit ids
        // and ignore structural/no-op actions that have no commit identity.
        let mut planned = Vec::new();
        for entry in todo.iter().filter(|entry| entry.action.is_commit()) {
            let expression = format!("{}^{{commit}}", entry.sha);
            planned.push(
                run_read(loc, &["rev-parse", "--verify", &expression])?
                    .trim()
                    .to_owned(),
            );
        }
        let expected_set: std::collections::HashSet<_> =
            expected.iter().map(String::as_str).collect();
        let planned_set: std::collections::HashSet<_> =
            planned.iter().map(String::as_str).collect();
        if expected_set != planned_set
            || expected_set.len() != expected.len()
            || planned_set.len() != planned.len()
        {
            bail!("branch advanced or todo does not exactly cover the range — reload and retry");
        }
    }
    let scratch_name = format!(
        "thegn-todo-{}-{}",
        std::process::id(),
        TODO_SCRATCH_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    let mut cleanup_path = None;
    let result = (|| {
        let scratch = loc
            .git_out(&["rev-parse", "--git-path", &scratch_name])
            .ok_or_else(|| anyhow!("cannot resolve todo scratch path"))?;
        let scratch = if scratch.starts_with('/') {
            scratch
        } else {
            format!("{}/{scratch}", loc.path())
        };
        match loc {
            GitLoc::Local(_) => {
                thegn_core::fsperm::write_owner_only_new(
                    std::path::Path::new(&scratch),
                    serialize_todo(todo).as_bytes(),
                )
                .context("create owner-only rebase todo scratch file")?;
                cleanup_path = Some(scratch.clone());
            }
            _ => {
                cleanup_path = Some(scratch.clone());
                if let Err(error) =
                    loc.write_git_path_new(&scratch_name, serialize_todo(todo).as_bytes())
                {
                    if error.kind() == std::io::ErrorKind::AlreadyExists {
                        cleanup_path = None;
                    }
                    tracing::debug!(target: "thegn::git", %error, "could not write prepared rebase todo scratch file");
                    return Err(error).context("write prepared rebase todo");
                }
            }
        }
        let editor = format!("cp {}", sh_quote(&scratch));
        let mut args = gpg_args(opts.override_gpg).to_vec();
        args.extend([
            "rebase",
            "-i",
            "--no-autosquash",
            "--empty=keep", // default --empty=stop would pause on empty commits
            base,
        ]);
        let envs = [
            ("GIT_SEQUENCE_EDITOR", editor.as_str()),
            ("GIT_EDITOR", ":"),
        ];
        classify(loc, run_w(loc, &envs, &args))
    })();
    match cleanup_path {
        Some(path) if matches!(loc, GitLoc::Local(_)) => match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                tracing::warn!(target: "thegn::git", %error, "could not remove prepared rebase todo scratch file")
            }
        },
        Some(path) => match loc
            .sh_command(&format!("rm -f -- {}", sh_quote(&path)))
            .status()
        {
            Ok(status) if status.success() => {}
            Ok(_) => {
                tracing::warn!(target: "thegn::git", "could not remove prepared rebase todo scratch file")
            }
            Err(error) => {
                tracing::warn!(target: "thegn::git", %error, "could not remove prepared rebase todo scratch file")
            }
        },
        None => {
            tracing::debug!(target: "thegn::git", "prepared rebase todo scratch file was not created; no cleanup needed")
        }
    }
    result
}

/// Map a rebase invocation result onto Done/Conflict/Paused by probing the
/// repo. Both exit statuses need the probe: `edit`/`break` stops exit ZERO
/// (a zero exit with `rebase-merge/` still present is a pause, not
/// completion), while conflicts exit non-zero; a non-zero exit with no
/// rebase in progress is a real error.
fn classify(loc: &GitLoc, res: Result<String>) -> Result<RebaseOutcome> {
    let rebasing = read_rebase_marker(loc, "rebase-merge/onto")?.is_some()
        || read_rebase_marker(loc, "rebase-apply/onto")?.is_some();
    match res {
        Ok(_) if !rebasing => Ok(RebaseOutcome::Done),
        Ok(_) => Ok(RebaseOutcome::Paused),
        Err(e) => {
            if !rebasing {
                return Err(e);
            }
            let conflicts = run_read(loc, &["diff", "--name-only", "--diff-filter=U"])
                .context("probe unmerged paths while classifying rebase")?;
            if !conflicts.trim().is_empty() {
                Ok(RebaseOutcome::Conflict)
            } else {
                Ok(RebaseOutcome::Paused)
            }
        }
    }
}

/// The base argument for rewriting history that includes `sha`: its parent,
/// or `--root` for a root commit.
fn base_for(loc: &GitLoc, sha: &str) -> Result<String> {
    let commit_expr = format!("{sha}^{{commit}}");
    let commit = run_read(loc, &["rev-parse", "--verify", "-q", &commit_expr])
        .with_context(|| format!("verify rebase target {sha}"))?;
    // `run_read` returns stdout verbatim, including rev-parse's trailing
    // newline. Pass the object id as one clean argument to rev-list.
    let commit = commit.trim();
    let parents = run_read(loc, &["rev-list", "--parents", "-n", "1", commit])
        .with_context(|| format!("read parents of rebase target {sha}"))?;
    let mut ids = parents.split_whitespace();
    let _verified_commit = ids
        .next()
        .ok_or_else(|| anyhow!("git returned no commit for {sha}"))?;
    match (ids.next(), ids.next()) {
        (Some(parent), None) => Ok(parent.to_owned()),
        (None, None) => Ok("--root".to_string()),
        _ => bail!("merge commits cannot be rebased here"),
    }
}

/// Generate the pick-everything todo for `base..HEAD` (oldest first).
fn todo_for(loc: &GitLoc, base: &str) -> Result<Vec<TodoEntry>> {
    let range = if base == "--root" {
        "HEAD".to_string()
    } else {
        format!("{base}..HEAD")
    };
    let out = run_read(loc, &["log", "--reverse", "--format=%x1f%H%x1f%s", &range])?;
    let todo = todo_from_log(&out)?;
    if todo.is_empty() {
        bail!("no commits to rebase");
    }
    // Merge commits cannot be linearized by a plain rebase -i; refuse with a
    // clear error instead of silently dropping them (no --rebase-merges yet).
    let merges = run_read(loc, &["log", "--merges", "--format=%h", &range])?;
    if !merges.trim().is_empty() {
        bail!("range contains merge commits — rebase them manually");
    }
    Ok(todo)
}

pub trait RebaseOps: GitBackend {
    /// The pick-everything todo for `<sha>^..HEAD` — the seed the
    /// interactive-rebase view edits before `rebase_interactive`.
    fn rebase_todo_for(&self, loc: &GitLoc, oldest_sha: &str) -> Result<Vec<TodoEntry>> {
        op::revision(oldest_sha)?;
        todo_for(loc, &base_for(loc, oldest_sha)?)
    }

    /// Build the interactive TODO for commits after an explicitly marked base.
    /// Unlike `rebase_todo_for`, the base may be any ancestor, not only the
    /// parent of the oldest selected commit.
    fn rebase_todo_since(&self, loc: &GitLoc, base_sha: &str) -> Result<Vec<TodoEntry>> {
        op::revision(base_sha)?;
        let base = run_read(
            loc,
            &["rev-parse", "--verify", &format!("{base_sha}^{{commit}}")],
        )
        .with_context(|| "marked base no longer exists; press B to re-mark")?;
        let head = run_read(loc, &["rev-parse", "--verify", "HEAD^{commit}"])?;
        if base.trim() == head.trim() {
            bail!("marked base is HEAD: no commits to rebase");
        }
        let (status, merge_base) = run_status(loc, &["merge-base", base.trim(), "HEAD"])?;
        if status == 1 {
            bail!("marked base is not an ancestor of HEAD; press B to re-mark");
        }
        if status != 0 {
            bail!("git merge-base failed with exit status {status}");
        }
        if merge_base.trim() != base.trim() {
            bail!("marked base is not an ancestor of HEAD; press B to re-mark");
        }
        todo_for(loc, base.trim())
    }

    /// Run an interactive rebase of `<base>..HEAD` with a caller-edited todo.
    fn rebase_interactive(
        &self,
        loc: &GitLoc,
        base: &str,
        todo: &[TodoEntry],
        opts: &RebaseOpts,
    ) -> Result<RebaseOutcome> {
        rebase_with_todo(loc, base, todo, opts)
    }

    /// Once-off: retag `targets` (squash/fixup/drop/edit) and rebase. For
    /// squash/fixup, the range includes the commit the oldest target folds
    /// into; when `oldest_sha` is itself a target, the range starts at its
    /// parent's parent.
    fn rebase_retag(
        &self,
        loc: &GitLoc,
        oldest_sha: &str,
        targets: &[&str],
        action: TodoAction,
        opts: &RebaseOpts,
    ) -> Result<RebaseOutcome> {
        rebase_retag_within(
            loc,
            oldest_sha,
            targets,
            action,
            opts,
            super::MUTATION_LOCK_WAIT,
        )
    }

    /// Once-off: move `sha` one position up (older) or down (newer).
    fn rebase_move(
        &self,
        loc: &GitLoc,
        sha: &str,
        up: bool,
        opts: &RebaseOpts,
    ) -> Result<RebaseOutcome> {
        with_rebase_txn(loc, &["rebase", "-i"], super::MUTATION_LOCK_WAIT, || {
            // Span one commit further back than the target so an upward move has
            // somewhere to go.
            let parent = base_for(loc, sha)?;
            let base = if parent == "--root" {
                parent
            } else {
                base_for(loc, &parent)?
            };
            let todo = todo_for(loc, &base)?;
            let todo = thegn_core::rebase_todo::move_entry(&todo, sha, up)?;
            rebase_with_todo(loc, &base, &todo, opts)
        })
    }

    /// Reword a commit. HEAD rewords amend in place; older commits stop at
    /// the target with `edit`, amend `--only` (so currently-staged changes
    /// are NOT swept into the commit), and continue. git refuses to start a
    /// rebase while the index has uncommitted changes, so staged changes are
    /// parked in a staged-only stash for the duration and restored — still
    /// staged — via `stash pop --index`.
    fn reword(&self, loc: &GitLoc, sha: &str, message: &str, opts: &RebaseOpts) -> Result<()> {
        reword_within(self, loc, sha, message, opts, super::MUTATION_LOCK_WAIT)
    }

    /// Amend an old commit with the currently staged changes: a fixup commit
    /// placed (by sha, immune to subject collisions) directly after its
    /// target, then an autosquash-style rebase.
    fn amend_old_commit(
        &self,
        loc: &GitLoc,
        target: &str,
        opts: &RebaseOpts,
    ) -> Result<RebaseOutcome> {
        with_rebase_txn(
            loc,
            &["commit", "--fixup"],
            super::MUTATION_LOCK_WAIT,
            || {
                let staged = loc
                    .git_command(&["diff", "--cached", "--quiet"])
                    .output()
                    .map(|o| !o.status.success())
                    .unwrap_or(false);
                if !staged {
                    bail!("nothing staged to amend into the commit");
                }
                let mut args = gpg_args(opts.override_gpg).to_vec();
                let fixup = format!("--fixup={target}");
                args.extend(["commit", "--no-verify", &fixup]);
                run_w(loc, &[], &args)?;
                let fixup_sha = loc
                    .git_out(&["rev-parse", "HEAD"])
                    .ok_or_else(|| anyhow!("no HEAD after fixup commit"))?;
                let base = base_for(loc, target)?;
                let todo = todo_for(loc, &base)?;
                let todo = place_fixup(&todo, &fixup_sha, target)?;
                rebase_with_todo(loc, &base, &todo, opts)
            },
        )
    }

    /// Rebase the current branch onto `target`, replaying only the commits
    /// after `marked_base` (`git rebase --onto`). `marked_base` is the
    /// lazygit "mark base commit" — typically the last commit shared with
    /// the old parent branch.
    fn rebase_onto(
        &self,
        loc: &GitLoc,
        target: &str,
        marked_base: &str,
        opts: &RebaseOpts,
    ) -> Result<RebaseOutcome> {
        let mut args = gpg_args(opts.override_gpg).to_vec();
        args.extend(["rebase", "--onto", target, marked_base]);
        classify(loc, run_w(loc, &[("GIT_EDITOR", ":")], &args))
    }

    /// Plain `git rebase <branch>` of the current branch.
    fn rebase_branch(
        &self,
        loc: &GitLoc,
        branch: &str,
        opts: &RebaseOpts,
    ) -> Result<RebaseOutcome> {
        let mut args = gpg_args(opts.override_gpg).to_vec();
        args.extend(["rebase", branch]);
        classify(loc, run_w(loc, &[("GIT_EDITOR", ":")], &args))
    }

    fn rebase_continue(&self, loc: &GitLoc) -> Result<RebaseOutcome> {
        classify(
            loc,
            run_w(loc, &[("GIT_EDITOR", ":")], &["rebase", "--continue"]),
        )
    }

    fn rebase_skip(&self, loc: &GitLoc) -> Result<RebaseOutcome> {
        classify(
            loc,
            run_w(loc, &[("GIT_EDITOR", ":")], &["rebase", "--skip"]),
        )
    }

    fn rebase_abort(&self, loc: &GitLoc) -> Result<()> {
        run_w(loc, &[], &["rebase", "--abort"]).map(|_| ())
    }

    /// The paused rebase's live state, or `None` when no interactive rebase
    /// is in progress.
    fn rebase_status(&self, loc: &GitLoc) -> Result<Option<RebaseStatus>> {
        let Some(onto) = read_rebase_marker(loc, "rebase-merge/onto")? else {
            return Ok(None);
        };
        let head_name = read_rebase_marker(loc, "rebase-merge/head-name")?
            .ok_or_else(|| anyhow!("rebase state is unreadable: missing head-name"))?;
        let remaining = read_rebase_marker(loc, "rebase-merge/git-rebase-todo")?
            .ok_or_else(|| anyhow!("rebase state is unreadable: missing pending todo"))?;
        let done = read_rebase_marker(loc, "rebase-merge/done")?.unwrap_or_default();
        let text = |b: Option<Vec<u8>>| {
            b.map(|b| String::from_utf8_lossy(&b).trim().to_string())
                .unwrap_or_default()
        };
        let conflicts = run_read(loc, &["diff", "--name-only", "--diff-filter=U"])
            .context("probe unmerged paths in rebase status")?;
        let conflicted = !conflicts.trim().is_empty();
        Ok(Some(RebaseStatus {
            onto: text(Some(onto)),
            head_name: text(Some(head_name))
                .trim_start_matches("refs/heads/")
                .to_string(),
            stopped_sha: read_rebase_marker(loc, "rebase-merge/stopped-sha")?
                .map(|bytes| String::from_utf8_lossy(&bytes).trim().to_string())
                .filter(|sha| !sha.is_empty()),
            done: parse_todo(&text(Some(done))),
            remaining: parse_todo(&text(Some(remaining))),
            paused: if conflicted {
                PauseReason::Conflict
            } else {
                PauseReason::Edit
            },
        }))
    }

    /// Rewrite the PENDING entries of a paused rebase (reorder/drop/retag
    /// mid-flight). The bounded advisory mutation lock excludes other thegn
    /// writers only; git takes no lock on this file, so an external editor can
    /// still race between the exact-byte check and atomic rename.
    fn rewrite_pending_todo(&self, loc: &GitLoc, todo: &[TodoEntry]) -> Result<()> {
        rewrite_live_todo_within(loc, todo, None, super::MUTATION_LOCK_WAIT)
    }

    /// [`rewrite_pending_todo`](Self::rewrite_pending_todo) with a
    /// lost-update guard: `baseline` is the pending list AS THE EDITOR READ
    /// IT, and the write is refused when the on-disk todo bytes differ from
    /// the bytes read before parsing. Local and remote writes both make an
    /// exact-byte final check under the bounded advisory mutation lock. The
    /// lock excludes other thegn writers only; git takes no lock on the todo,
    /// so an external editor can still race between check and rename.
    fn rewrite_pending_todo_checked(
        &self,
        loc: &GitLoc,
        todo: &[TodoEntry],
        baseline: &[TodoEntry],
    ) -> Result<()> {
        rewrite_live_todo_within(loc, todo, Some(baseline), super::MUTATION_LOCK_WAIT)
    }
}

fn rebase_retag_within(
    loc: &GitLoc,
    oldest_sha: &str,
    targets: &[&str],
    action: TodoAction,
    opts: &RebaseOpts,
    wait: Duration,
) -> Result<RebaseOutcome> {
    with_rebase_txn(loc, &["rebase", "-i"], wait, || {
        let is_fold = matches!(
            &action,
            TodoAction::Squash | TodoAction::Fixup | TodoAction::FixupC
        );
        let is_target = if is_fold {
            let oldest_id = run_read(
                loc,
                &["rev-parse", "--verify", &format!("{oldest_sha}^{{commit}}")],
            )?;
            let mut found = false;
            for target in targets {
                let target_id = run_read(
                    loc,
                    &["rev-parse", "--verify", &format!("{target}^{{commit}}")],
                )?;
                if target_id.trim() == oldest_id.trim() {
                    found = true;
                    break;
                }
            }
            found
        } else {
            false
        };
        let base = if is_target {
            let parent = base_for(loc, oldest_sha)?;
            if parent == "--root" {
                bail!("cannot squash/fixup the root commit: it has no previous commit");
            }
            base_for(loc, &parent)?
        } else {
            base_for(loc, oldest_sha)?
        };
        let todo = todo_for(loc, &base)?;
        let todo = retag(&todo, targets, action)?;
        rebase_with_todo(loc, &base, &todo, opts)
    })
}

fn reword_within<T: RebaseOps + ?Sized>(
    backend: &T,
    loc: &GitLoc,
    sha: &str,
    message: &str,
    opts: &RebaseOpts,
    wait: Duration,
) -> Result<()> {
    with_rebase_txn(loc, &["rebase", "-i"], wait, || {
        op::revision(sha)?;
        let head = loc
            .git_out(&["rev-parse", "HEAD"])
            .ok_or_else(|| anyhow!("no HEAD"))?;
        let is_head = head.starts_with(sha) || sha.starts_with(&head);
        let amend = |loc: &GitLoc| -> Result<()> {
            let mut args = gpg_args(opts.override_gpg).to_vec();
            args.extend([
                "commit",
                "--amend",
                "--only",
                "--allow-empty",
                "--no-verify",
                "-m",
                message,
            ]);
            run_w(loc, &[], &args).map(|_| ())
        };
        if is_head {
            return amend(loc);
        }
        let staged = loc
            .git_command(&["diff", "--cached", "--quiet"])
            .output()
            .map(|o| !o.status.success())
            .unwrap_or(false);
        if staged {
            let args = ["stash", "push", "--staged", "--quiet", "-m", "thegn reword"];
            run_w(loc, &[], &args).context("park staged changes for the reword rebase")?;
        }
        let rewrite = || -> Result<()> {
            let base = base_for(loc, sha)?;
            let todo = todo_for(loc, &base)?;
            let todo = retag(&todo, &[sha], TodoAction::Edit)?;
            match rebase_with_todo(loc, &base, &todo, opts)? {
                RebaseOutcome::Paused => {}
                RebaseOutcome::Conflict => bail!("rebase hit a conflict before the reword stop"),
                RebaseOutcome::Done => bail!("rebase did not stop at the commit to reword"),
            }
            amend(loc)?;
            match backend.rebase_continue(loc)? {
                RebaseOutcome::Done => Ok(()),
                RebaseOutcome::Conflict => {
                    bail!("conflict while replaying commits after the reword")
                }
                RebaseOutcome::Paused => bail!("rebase paused unexpectedly after the reword"),
            }
        };
        let res = rewrite();
        if staged {
            // Best-effort on the error path (a conflicted worktree cannot
            // take the pop); the stash stays recoverable either way.
            let pop = run_w(loc, &[], &["stash", "pop", "--index", "--quiet"]);
            if res.is_ok() {
                pop.context("restore parked staged changes after the reword")?;
            }
        }
        res
    })
}

fn rewrite_live_todo_within(
    loc: &GitLoc,
    todo: &[TodoEntry],
    baseline: Option<&[TodoEntry]>,
    wait: Duration,
) -> Result<()> {
    let bytes = serialize_todo(todo);
    if let GitLoc::Local(worktree) = loc {
        let _lock =
            super::require_mutation_lock_within(worktree, &["rebase-todo", "rewrite"], wait)?;
        let Some((path, disk)) = read_local_pending_todo(loc)? else {
            bail!("no interactive rebase in progress");
        };
        if let Some(baseline) = baseline {
            let text = String::from_utf8(disk.clone())
                .map_err(|_| anyhow!("rebase todo is not valid UTF-8; refusing a lossy rewrite"))?;
            if parse_todo(&text) != baseline {
                bail!("rebase todo changed on disk — reload the editor before rewriting");
            }
        }
        return replace_local_pending_todo(&path, &disk, bytes.as_bytes())
            .context("rewrite live rebase todo");
    }
    let Some(disk) = read_rebase_marker(loc, "rebase-merge/git-rebase-todo")? else {
        bail!("no interactive rebase in progress");
    };
    if let Some(baseline) = baseline {
        let text = String::from_utf8(disk.clone())
            .map_err(|_| anyhow!("rebase todo is not valid UTF-8; refusing a lossy rewrite"))?;
        if parse_todo(&text) != baseline {
            bail!("rebase todo changed on disk — reload the editor before rewriting");
        }
    }
    loc.write_git_path_if_unchanged("rebase-merge/git-rebase-todo", &disk, bytes.as_bytes())
        .context("rewrite live rebase todo")
}

impl<T: GitBackend + ?Sized> RebaseOps for T {}

#[cfg(test)]
mod tests {
    use super::super::testutil::{TestRepo, git_in};
    use super::super::{CliGit, MergeKind};
    use super::*;

    /// A throwaway repo with local identity and deterministic rebase
    /// settings: the ops under test shell out with the ambient environment,
    /// so the developer's real global config (gpg signing, autostash) must
    /// be overridden repo-locally for hermetic runs.
    fn repo(tag: &str) -> TestRepo {
        let r = TestRepo::new(tag);
        for (k, v) in [
            ("user.name", "t"),
            ("user.email", "t@e"),
            ("commit.gpgsign", "false"),
            ("rebase.autostash", "false"),
        ] {
            git_in(&r.dir, &["config", k, v]);
        }
        r
    }

    /// c1..c4, each adding a distinct file — reorders/squashes are
    /// conflict-free and every commit carries tree content.
    fn linear(tag: &str) -> TestRepo {
        let r = repo(tag);
        r.commit_file("f1.txt", "1\n", "c1");
        r.commit_file("f2.txt", "2\n", "c2");
        r.commit_file("f3.txt", "3\n", "c3");
        r.commit_file("f4.txt", "4\n", "c4");
        r
    }

    /// c2 and c3 rewrite the same line of the same file (any rewrite that
    /// detaches c3 from c2 conflicts); c4 is independent.
    fn conflicting(tag: &str) -> TestRepo {
        let r = repo(tag);
        r.commit_file("f.txt", "base\n", "c1");
        r.commit_file("f.txt", "two\n", "c2");
        r.commit_file("f.txt", "three\n", "c3");
        r.commit_file("f4.txt", "4\n", "c4");
        r
    }

    fn opts() -> RebaseOpts {
        RebaseOpts::default()
    }

    #[test]
    fn rebase_txn_retag_refuses_while_another_thread_holds_the_lock() {
        let r = linear("rebase-retag-txn");
        let loc = r.loc();
        let head = r.head();
        let target = r.out(&["rev-parse", "HEAD~1"]).trim().to_owned();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let worktree = r.dir.clone();
        let holder = std::thread::spawn(move || {
            let txn =
                thegn_core::util::begin_git_mutation(&worktree, Duration::from_secs(5)).unwrap();
            ready_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            drop(txn);
        });
        ready_rx.recv().unwrap();

        let error = rebase_retag_within(
            &loc,
            &target,
            &[&target],
            TodoAction::Drop,
            &opts(),
            Duration::from_millis(100),
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("refusing git rebase -i: repository mutation lock"),
            "{error:#}"
        );
        assert_eq!(r.head(), head);

        release_tx.send(()).unwrap();
        holder.join().unwrap();
    }

    #[test]
    fn rebase_txn_reword_refuses_while_another_thread_holds_the_lock() {
        let r = linear("rebase-reword-txn");
        let loc = r.loc();
        std::fs::write(r.dir.join("staged.txt"), "staged\n").unwrap();
        git_in(&r.dir, &["add", "staged.txt"]);
        let head = r.head();
        let target = r.out(&["rev-parse", "HEAD~1"]).trim().to_owned();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let worktree = r.dir.clone();
        let holder = std::thread::spawn(move || {
            let txn =
                thegn_core::util::begin_git_mutation(&worktree, Duration::from_secs(5)).unwrap();
            ready_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            drop(txn);
        });
        ready_rx.recv().unwrap();

        let error = reword_within(
            &CliGit,
            &loc,
            &target,
            "rewritten",
            &opts(),
            Duration::from_millis(100),
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("refusing git rebase -i: repository mutation lock"),
            "{error:#}"
        );
        assert_eq!(r.head(), head);
        assert!(CliGit.stash_list(&loc).unwrap().is_empty());

        release_tx.send(()).unwrap();
        holder.join().unwrap();
    }

    fn flock_available() -> bool {
        std::process::Command::new("sh")
            .args(["-c", "command -v flock >/dev/null 2>&1"])
            .status()
            .is_ok_and(|status| status.success())
    }

    /// File names in HEAD's tree.
    fn tree(r: &TestRepo) -> String {
        r.out(&["ls-tree", "--name-only", "HEAD"])
    }

    /// File names touched by `sha`.
    fn touched(r: &TestRepo, sha: &str) -> String {
        r.out(&["show", "--name-only", "--format=", sha])
    }

    #[test]
    fn rebase_bails_when_head_advances_after_the_todo_is_built() {
        // Build the pick-everything todo for c2^..HEAD, then let a new commit land
        // on the branch (as a concurrent agent would). Applying the stale todo must
        // refuse — git rebase would otherwise silently drop c5 from history.
        let r = linear("head-advance");
        let c2 = r.sha_of("c2");
        let base = base_for(&r.loc(), &c2).unwrap();
        let todo = CliGit.rebase_todo_for(&r.loc(), &c2).unwrap();

        // HEAD advances out from under the built todo.
        r.commit_file("f5.txt", "5\n", "c5");
        let head_before = r.out(&["rev-parse", "HEAD"]);

        let err = CliGit
            .rebase_interactive(&r.loc(), &base, &todo, &opts())
            .unwrap_err();
        assert!(
            err.to_string().contains("branch advanced"),
            "expected a branch-advanced bail, got: {err}"
        );
        // The branch is untouched — c5 is still there.
        assert_eq!(r.out(&["rev-parse", "HEAD"]), head_before);
        assert_eq!(r.subjects(), ["c5", "c4", "c3", "c2", "c1"]);
    }

    #[test]
    fn rebase_todo_since_plans_commits_after_the_mark() {
        let r = repo("marked-base-todo");
        r.commit_file("c0.txt", "0\n", "c0");
        r.commit_file("c1.txt", "1\n", "c1");
        r.commit_file("c2.txt", "2\n", "c2");
        r.commit_file("c3.txt", "3\n", "c3");
        let c1 = r.sha_of("c1");
        let todo = CliGit.rebase_todo_since(&r.loc(), &c1).unwrap();
        assert_eq!(
            todo.iter()
                .map(|entry| entry.subject.as_str())
                .collect::<Vec<_>>(),
            ["c2", "c3"]
        );
    }

    #[test]
    fn rebase_todo_since_refuses_head_and_non_ancestors() {
        let r = linear("marked-base-invalid");
        let head = r.out(&["rev-parse", "HEAD"]);
        let head_error = CliGit.rebase_todo_since(&r.loc(), &head).unwrap_err();
        assert!(head_error.to_string().contains("marked base is HEAD"));

        let c1 = r.sha_of("c1");
        git_in(&r.dir, &["checkout", "-b", "sibling", &c1]);
        r.commit_file("sibling.txt", "sibling\n", "sibling");
        let sibling = r.out(&["rev-parse", "HEAD"]);
        git_in(&r.dir, &["checkout", "-"]);
        let sibling_error = CliGit.rebase_todo_since(&r.loc(), &sibling).unwrap_err();
        assert!(sibling_error.to_string().contains("not an ancestor"));
    }

    #[test]
    fn rebase_todo_since_explains_when_the_mark_is_missing() {
        let r = linear("marked-base-missing");
        let missing = "0123456789012345678901234567890123456789";
        let error = CliGit.rebase_todo_since(&r.loc(), missing).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("marked base no longer exists; press B to re-mark")
        );
    }

    #[test]
    fn rebase_bails_when_todo_duplicates_a_commit() {
        let r = linear("todo-range-mismatch");
        let c2 = r.sha_of("c2");
        let base = base_for(&r.loc(), &c2).unwrap();
        let mut todo = CliGit.rebase_todo_for(&r.loc(), &c2).unwrap();
        todo.push(todo[0].clone());
        let before = r.out(&["rev-parse", "HEAD"]);
        let error = CliGit
            .rebase_interactive(&r.loc(), &base, &todo, &opts())
            .unwrap_err();
        assert!(error.to_string().contains("exactly cover"));
        assert_eq!(r.out(&["rev-parse", "HEAD"]), before);
    }

    #[test]
    fn interactive_rebase_resolves_panel_style_short_sha_todos() {
        let r = linear("short-sha-todo");
        let c2 = r.sha_of("c2");
        let base = base_for(&r.loc(), &c2).unwrap();
        let mut todo = CliGit.rebase_todo_for(&r.loc(), &c2).unwrap();
        for entry in &mut todo {
            entry.sha.truncate(8);
        }

        assert_eq!(
            CliGit
                .rebase_interactive(&r.loc(), &base, &todo, &opts())
                .unwrap(),
            RebaseOutcome::Done
        );
        assert_eq!(r.subjects(), ["c4", "c3", "c2", "c1"]);
    }

    #[test]
    fn base_for_uses_root_only_for_a_verified_root_commit() {
        let r = repo("base-for-root");
        r.commit_file("root.txt", "root\n", "root");
        let root = r.sha_of("root");
        assert_eq!(base_for(&r.loc(), &root).unwrap(), "--root");
        r.commit_file("child.txt", "child\n", "child");
        let child = r.sha_of("child");
        assert_eq!(base_for(&r.loc(), &child).unwrap(), root);
        assert!(base_for(&r.loc(), "deadbeef-deadbeef").is_err());
    }

    #[test]
    fn base_for_explains_that_merge_targets_are_unsupported() {
        let r = repo("base-for-merge");
        r.commit_file("base.txt", "base\n", "base");
        r.out(&["checkout", "-q", "-b", "topic"]);
        r.commit_file("topic.txt", "topic\n", "topic");
        r.out(&["checkout", "-q", "main"]);
        r.commit_file("main.txt", "main\n", "main");
        r.out(&["merge", "--no-ff", "topic", "-m", "merge"]);
        let merge = r.out(&["rev-parse", "HEAD"]);

        let error = base_for(&r.loc(), merge.trim()).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("merge commits cannot be rebased here")
        );
    }

    #[test]
    fn unreadable_local_rebase_marker_is_an_error_not_absence() {
        let r = repo("unreadable-rebase-marker");
        std::fs::create_dir_all(r.dir.join(".git/rebase-merge/onto")).unwrap();
        let error = read_rebase_marker(&r.loc(), "rebase-merge/onto")
            .expect_err("a directory at the marker path is unreadable, not absent");
        assert!(error.to_string().contains("read sequencer marker"));
    }

    #[test]
    fn rebase_status_before_first_command_reports_empty_done() {
        let r = linear("rebase-status-no-done");
        let c2 = r.sha_of("c2");
        let c3 = r.sha_of("c3");
        let rel = r.out(&["rev-parse", "--git-path", "rebase-merge"]);
        let rebase_dir = r.dir.join(rel.trim());
        std::fs::create_dir_all(&rebase_dir).unwrap();
        std::fs::write(rebase_dir.join("onto"), format!("{c2}\n")).unwrap();
        std::fs::write(rebase_dir.join("head-name"), "refs/heads/main\n").unwrap();
        std::fs::write(
            rebase_dir.join("git-rebase-todo"),
            format!("pick {c3} c3\n"),
        )
        .unwrap();

        let status = CliGit
            .rebase_status(&r.loc())
            .unwrap()
            .expect("rebase markers indicate an active rebase");
        assert!(status.done.is_empty());
        assert_eq!(status.remaining.len(), 1);
    }

    #[test]
    fn unreadable_remote_rebase_marker_is_an_error_not_absence() {
        let r = repo("unreadable-remote-rebase-marker");
        let loc = GitLoc::provider(vec!["env".into()], r.dir.to_string_lossy());
        assert_eq!(
            read_rebase_marker(&loc, "rebase-merge/onto").unwrap(),
            None,
            "a missing marker is absent"
        );
        std::fs::create_dir_all(r.dir.join(".git/rebase-merge/onto")).unwrap();
        let error = read_rebase_marker(&loc, "rebase-merge/onto")
            .expect_err("a directory at the marker path is unreadable, not absent");
        assert!(error.to_string().contains("read sequencer marker"));
        assert!(CliGit.rebase_status(&loc).is_err());
    }

    #[test]
    fn drop_removes_the_commit_and_its_change() {
        let r = linear("drop");
        let c2 = r.sha_of("c2");
        let out = CliGit
            .rebase_retag(&r.loc(), &c2, &[&c2], TodoAction::Drop, &opts())
            .unwrap();
        assert_eq!(out, RebaseOutcome::Done);
        assert_eq!(r.subjects(), ["c4", "c3", "c1"]);
        let t = tree(&r);
        assert!(!t.contains("f2.txt"), "dropped change still in tree: {t}");
        assert!(t.contains("f3.txt") && t.contains("f4.txt"), "{t}");
    }

    #[test]
    fn fixup_folds_into_predecessor_keeping_its_message() {
        let r = linear("fixup");
        let c2 = r.sha_of("c2");
        let c3 = r.sha_of("c3");
        let out = CliGit
            .rebase_retag(&r.loc(), &c2, &[&c3], TodoAction::Fixup, &opts())
            .unwrap();
        assert_eq!(out, RebaseOutcome::Done);
        assert_eq!(r.subjects(), ["c4", "c2", "c1"]);
        let combined = r.sha_of("c2");
        let files = touched(&r, &combined);
        assert!(
            files.contains("f2.txt") && files.contains("f3.txt"),
            "{files}"
        );
        // Fixup keeps the TARGET's message verbatim.
        assert_eq!(r.out(&["log", "--format=%B", "-n1", &combined]), "c2");
        assert!(tree(&r).contains("f3.txt"));
    }

    #[test]
    fn squash_combines_trees_and_messages() {
        let r = linear("squash");
        let c2 = r.sha_of("c2");
        let c3 = r.sha_of("c3");
        let out = CliGit
            .rebase_retag(&r.loc(), &c2, &[&c3], TodoAction::Squash, &opts())
            .unwrap();
        assert_eq!(out, RebaseOutcome::Done);
        assert_eq!(r.subjects(), ["c4", "c2", "c1"]);
        let combined = r.sha_of("c2");
        // GIT_EDITOR=: accepted the default combined message: both subjects.
        let body = r.out(&["log", "--format=%B", "-n1", &combined]);
        assert!(body.contains("c2") && body.contains("c3"), "{body}");
        let files = touched(&r, &combined);
        assert!(
            files.contains("f2.txt") && files.contains("f3.txt"),
            "{files}"
        );
    }

    #[test]
    fn fixup_of_head_folds_into_its_parent() {
        let r = linear("fixup-head-target");
        let c4 = r.sha_of("c4");
        assert_eq!(
            CliGit
                .rebase_retag(&r.loc(), &c4, &[&c4], TodoAction::Fixup, &opts())
                .unwrap(),
            RebaseOutcome::Done
        );
        assert_eq!(r.subjects(), ["c3", "c2", "c1"]);
        let files = touched(&r, &r.sha_of("c3"));
        assert!(
            files.contains("f3.txt") && files.contains("f4.txt"),
            "{files}"
        );
        let rebase_dir = r.out(&["rev-parse", "--git-path", "rebase-merge"]);
        assert!(!r.dir.join(rebase_dir.trim()).exists());
    }

    #[test]
    fn squash_of_head_folds_into_its_parent() {
        let r = linear("squash-head-target");
        let c4 = r.sha_of("c4");
        assert_eq!(
            CliGit
                .rebase_retag(&r.loc(), &c4, &[&c4], TodoAction::Squash, &opts())
                .unwrap(),
            RebaseOutcome::Done
        );
        assert_eq!(r.subjects(), ["c3", "c2", "c1"]);
        let files = touched(&r, &r.sha_of("c3"));
        assert!(
            files.contains("f3.txt") && files.contains("f4.txt"),
            "{files}"
        );
        let rebase_dir = r.out(&["rev-parse", "--git-path", "rebase-merge"]);
        assert!(!r.dir.join(rebase_dir.trim()).exists());
    }

    #[test]
    fn squash_of_root_commit_is_refused_without_touching_the_repo() {
        let r = linear("squash-root-target");
        let c1 = r.sha_of("c1");
        let head = r.head();
        let error = CliGit
            .rebase_retag(&r.loc(), &c1, &[&c1], TodoAction::Squash, &opts())
            .unwrap_err();
        assert!(error.to_string().contains("root commit"), "{error}");
        assert_eq!(r.head(), head);
        let rebase_dir = r.out(&["rev-parse", "--git-path", "rebase-merge"]);
        assert!(!r.dir.join(rebase_dir.trim()).exists());
    }

    #[test]
    fn squash_target_with_merge_parent_is_refused_without_starting_rebase() {
        let r = repo("squash-merge-parent");
        r.commit_file("base.txt", "base\n", "base");
        r.out(&["checkout", "-q", "-b", "topic"]);
        r.commit_file("topic.txt", "topic\n", "topic");
        r.out(&["checkout", "-q", "main"]);
        r.commit_file("main.txt", "main\n", "main");
        r.out(&["merge", "--no-ff", "topic", "-m", "merge"]);
        r.commit_file("child.txt", "child\n", "child");
        let child = r.sha_of("child");
        let head = r.head();

        let error = CliGit
            .rebase_retag(&r.loc(), &child, &[&child], TodoAction::Squash, &opts())
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("merge commits cannot be rebased here")
        );
        assert_eq!(r.head(), head);
        let rebase_dir = r.out(&["rev-parse", "--git-path", "rebase-merge"]);
        assert!(!r.dir.join(rebase_dir.trim()).exists());
    }

    #[test]
    fn todo_starting_with_squash_is_refused_before_git_runs() {
        let r = linear("squash-first-todo");
        let c2 = r.sha_of("c2");
        let c3 = r.sha_of("c3");
        let c4 = r.sha_of("c4");
        let base = base_for(&r.loc(), &c3).unwrap();
        let todo = parse_todo(&format!("squash {c3} c3\npick {c4} c4\n"));
        let head = r.head();

        let error = CliGit
            .rebase_interactive(&r.loc(), &base, &todo, &opts())
            .unwrap_err();
        assert!(error.to_string().contains("first commit"), "{error}");
        assert_eq!(r.head(), head);
        assert_eq!(r.sha_of("c2"), c2);
        let rebase_dir = r.out(&["rev-parse", "--git-path", "rebase-merge"]);
        assert!(!r.dir.join(rebase_dir.trim()).exists());
    }

    #[test]
    fn move_up_swaps_order_without_changing_the_tree() {
        let r = linear("move");
        let old_head = r.head();
        let c3 = r.sha_of("c3");
        let out = CliGit.rebase_move(&r.loc(), &c3, true, &opts()).unwrap();
        assert_eq!(out, RebaseOutcome::Done);
        assert_eq!(r.subjects(), ["c4", "c2", "c3", "c1"]);
        // Same end-state tree, only the history order changed.
        assert_eq!(r.out(&["diff", "--stat", &old_head, "HEAD"]), "");
    }

    #[test]
    fn reword_changes_the_message_and_preserves_staged_changes() {
        let r = linear("reword");
        let old_head = r.head();
        std::fs::write(r.dir.join("junk.txt"), "junk\n").unwrap();
        git_in(&r.dir, &["add", "junk.txt"]);

        // Non-HEAD reword: staged-but-unrelated changes survive, still
        // staged-only, and are NOT swept into the rewritten commit.
        let c2 = r.sha_of("c2");
        CliGit
            .reword(&r.loc(), &c2, "c2 reworded", &opts())
            .unwrap();
        assert_eq!(r.subjects(), ["c4", "c3", "c2 reworded", "c1"]);
        assert_eq!(r.out(&["status", "--porcelain"]), "A  junk.txt");
        let files = touched(&r, &r.sha_of("c2 reworded"));
        assert!(!files.contains("junk.txt"), "staged file swept in: {files}");
        assert_eq!(r.out(&["diff", "--stat", &old_head, "HEAD"]), "");

        // HEAD reword amends in place; `--only` keeps the junk out.
        let head = r.head();
        CliGit
            .reword(&r.loc(), &head, "c4 reworded", &opts())
            .unwrap();
        assert_eq!(r.subjects()[0], "c4 reworded");
        assert_eq!(r.out(&["status", "--porcelain"]), "A  junk.txt");
        let files = touched(&r, "HEAD");
        assert!(!files.contains("junk.txt"), "staged file swept in: {files}");
    }

    #[test]
    fn amend_old_commit_targets_by_sha_not_subject() {
        let r = repo("amend");
        r.commit_file("f1.txt", "1\n", "c1");
        r.commit_file("f2.txt", "2\n", "dup");
        let older_dup = r.head();
        r.commit_file("f3.txt", "3\n", "c3");
        r.commit_file("f4.txt", "4\n", "dup");

        // Nothing staged → refused before anything happens.
        let err = CliGit
            .amend_old_commit(&r.loc(), &older_dup, &opts())
            .unwrap_err();
        assert!(err.to_string().contains("nothing staged"), "{err}");

        std::fs::write(r.dir.join("amend.txt"), "amended\n").unwrap();
        git_in(&r.dir, &["add", "amend.txt"]);
        let out = CliGit
            .amend_old_commit(&r.loc(), &older_dup, &opts())
            .unwrap();
        assert_eq!(out, RebaseOutcome::Done);
        // The fixup! commit folded away; both "dup" subjects survive.
        assert_eq!(r.subjects(), ["dup", "c3", "dup", "c1"]);
        // sha-addressed placement: the OLDER dup gained the change (subject
        // matching — autosquash semantics — would have hit the newer one).
        let log = r.out(&["log", "--format=%H %s"]);
        let dups: Vec<&str> = log
            .lines()
            .filter(|l| l.ends_with(" dup"))
            .map(|l| l.split(' ').next().unwrap())
            .collect();
        assert_eq!(dups.len(), 2);
        let (newer, older) = (dups[0], dups[1]);
        let older_files = touched(&r, older);
        assert!(older_files.contains("amend.txt"), "{older_files}");
        assert!(older_files.contains("f2.txt"), "{older_files}");
        assert!(
            !touched(&r, newer).contains("amend.txt"),
            "newer dup gained the amend"
        );
        assert_eq!(r.out(&["status", "--porcelain"]), "");
    }

    #[test]
    fn conflict_status_abort_roundtrip() {
        let r = conflicting("conflict");
        let old_head = r.head();
        let c2 = r.sha_of("c2");
        // Dropping c2 forces c3 (a rewrite of c2's line) onto c1 → conflict.
        let out = CliGit
            .rebase_retag(&r.loc(), &c2, &[&c2], TodoAction::Drop, &opts())
            .unwrap();
        assert_eq!(out, RebaseOutcome::Conflict);

        let st = CliGit
            .rebase_status(&r.loc())
            .unwrap()
            .expect("mid-rebase status");
        assert_eq!(st.paused, PauseReason::Conflict);
        assert_eq!(st.head_name, "main");
        assert!(st.stopped_sha.is_some());
        assert!(!st.done.is_empty(), "done: {:?}", st.done);
        assert!(
            st.remaining.iter().any(|e| e.subject == "c4"),
            "remaining: {:?}",
            st.remaining
        );

        let merge = CliGit.merge_state(&r.loc()).unwrap().expect("merge state");
        assert_eq!(merge.kind, MergeKind::Rebase);

        CliGit.rebase_abort(&r.loc()).unwrap();
        assert_eq!(r.head(), old_head, "abort must restore the exact HEAD");
        assert!(CliGit.rebase_status(&r.loc()).unwrap().is_none());
        assert!(CliGit.merge_state(&r.loc()).unwrap().is_none());
    }

    #[test]
    fn conflict_resolve_continue_completes() {
        let r = conflicting("resolve");
        let c2 = r.sha_of("c2");
        let out = CliGit
            .rebase_retag(&r.loc(), &c2, &[&c2], TodoAction::Drop, &opts())
            .unwrap();
        assert_eq!(out, RebaseOutcome::Conflict);

        std::fs::write(r.dir.join("f.txt"), "three\n").unwrap();
        git_in(&r.dir, &["add", "f.txt"]);
        let out = CliGit.rebase_continue(&r.loc()).unwrap();
        assert_eq!(out, RebaseOutcome::Done);
        assert_eq!(r.subjects(), ["c4", "c3", "c1"]);
        assert_eq!(
            std::fs::read_to_string(r.dir.join("f.txt")).unwrap(),
            "three\n"
        );
        assert_eq!(r.out(&["status", "--porcelain"]), "");
    }

    #[test]
    fn edit_pause_supports_rewriting_the_pending_todo() {
        let r = linear("edit");
        let c2 = r.sha_of("c2");
        let out = CliGit
            .rebase_retag(&r.loc(), &c2, &[&c2], TodoAction::Edit, &opts())
            .unwrap();
        assert_eq!(out, RebaseOutcome::Paused);

        let st = CliGit
            .rebase_status(&r.loc())
            .unwrap()
            .expect("paused status");
        assert_eq!(st.paused, PauseReason::Edit);
        let picks: Vec<&str> = st.remaining.iter().map(|e| e.subject.as_str()).collect();
        assert_eq!(picks, ["c3", "c4"]);

        // Drop pending c3 mid-flight, then finish.
        let kept: Vec<TodoEntry> = st
            .remaining
            .iter()
            .filter(|e| e.subject != "c3")
            .cloned()
            .collect();
        CliGit.rewrite_pending_todo(&r.loc(), &kept).unwrap();
        let out = CliGit.rebase_continue(&r.loc()).unwrap();
        assert_eq!(out, RebaseOutcome::Done);
        assert_eq!(r.subjects(), ["c4", "c2", "c1"]);
        assert!(!tree(&r).contains("f3.txt"));

        // Outside a rebase the live-todo rewrite is refused.
        let err = CliGit.rewrite_pending_todo(&r.loc(), &kept).unwrap_err();
        assert!(err.to_string().contains("no interactive rebase"), "{err}");
    }

    #[test]
    fn unreadable_pending_todo_is_not_reported_as_no_rebase() {
        let r = linear("edit-unreadable-todo");
        let c2 = r.sha_of("c2");
        let out = CliGit
            .rebase_retag(&r.loc(), &c2, &[&c2], TodoAction::Edit, &opts())
            .unwrap();
        assert_eq!(out, RebaseOutcome::Paused);

        let todo_path = r.dir.join(".git/rebase-merge/git-rebase-todo");
        std::fs::remove_file(&todo_path).unwrap();
        std::fs::create_dir(&todo_path).unwrap();
        let todo = Vec::new();

        let error = CliGit
            .rewrite_pending_todo(&r.loc(), &todo)
            .expect_err("unreadable todo must fail");
        assert!(
            !error.to_string().contains("no interactive rebase"),
            "{error}"
        );
        let error = CliGit
            .rewrite_pending_todo_checked(&r.loc(), &todo, &todo)
            .expect_err("unreadable todo must fail");
        assert!(
            !error.to_string().contains("no interactive rebase"),
            "{error}"
        );
    }

    #[test]
    fn checked_rewrite_refuses_a_stale_baseline_and_accepts_a_fresh_one() {
        let r = linear("edit-checked");
        let c2 = r.sha_of("c2");
        let out = CliGit
            .rebase_retag(&r.loc(), &c2, &[&c2], TodoAction::Edit, &opts())
            .unwrap();
        assert_eq!(out, RebaseOutcome::Paused);
        let st = CliGit
            .rebase_status(&r.loc())
            .unwrap()
            .expect("paused status");
        let baseline = st.remaining.clone();

        // Someone edits the live todo from another terminal mid-pause…
        let externally_edited: Vec<TodoEntry> = baseline
            .iter()
            .filter(|e| e.subject != "c4")
            .cloned()
            .collect();
        CliGit
            .rewrite_pending_todo(&r.loc(), &externally_edited)
            .unwrap();

        // …so a rewrite from the now-stale editor is refused, clobbering
        // nothing.
        let kept: Vec<TodoEntry> = baseline
            .iter()
            .filter(|e| e.subject != "c3")
            .cloned()
            .collect();
        let err = CliGit
            .rewrite_pending_todo_checked(&r.loc(), &kept, &baseline)
            .unwrap_err();
        assert!(err.to_string().contains("changed on disk"), "{err}");
        let st = CliGit.rebase_status(&r.loc()).unwrap().unwrap();
        assert_eq!(st.remaining, externally_edited, "external edit survived");

        // Re-reading the live todo makes the rewrite valid again (and the
        // parsed-entry comparison shrugs off git's regenerated comments).
        let fresh = st.remaining;
        let kept: Vec<TodoEntry> = fresh
            .iter()
            .filter(|e| e.subject != "c3")
            .cloned()
            .collect();
        CliGit
            .rewrite_pending_todo_checked(&r.loc(), &kept, &fresh)
            .unwrap();
        let out = CliGit.rebase_continue(&r.loc()).unwrap();
        assert_eq!(out, RebaseOutcome::Done);
        assert_eq!(
            r.subjects(),
            ["c2", "c1"],
            "c3 dropped + c4 dropped externally"
        );
    }

    #[test]
    fn checked_rewrite_requires_mutation_lock_and_releases_it_after_publish() {
        let r = linear("edit-lock");
        let c2 = r.sha_of("c2");
        let out = CliGit
            .rebase_retag(&r.loc(), &c2, &[&c2], TodoAction::Edit, &opts())
            .unwrap();
        assert_eq!(out, RebaseOutcome::Paused);
        let status = CliGit
            .rebase_status(&r.loc())
            .unwrap()
            .expect("paused status");
        let baseline = status.remaining;
        let kept: Vec<TodoEntry> = baseline
            .iter()
            .filter(|entry| entry.subject != "c3")
            .cloned()
            .collect();
        let todo_path = r.dir.join(".git/rebase-merge/git-rebase-todo");
        let before = std::fs::read(&todo_path).unwrap();
        let held = thegn_core::util::try_lock_git_mutations(&r.dir).unwrap();
        let loc = r.loc();
        let baseline_for_worker = baseline.clone();
        let kept_for_worker = kept.clone();
        let (sender, receiver) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let result = rewrite_live_todo_within(
                &loc,
                &kept_for_worker,
                Some(&baseline_for_worker),
                Duration::from_millis(200),
            )
            .map_err(|error| error.to_string());
            sender.send(result).unwrap();
        });
        let error = receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("bounded mutation-lock wait must return promptly")
            .expect_err("rewrite must refuse while another thegn writer holds the lock");
        assert!(error.contains("mutation lock"), "{error}");
        worker.join().unwrap();
        assert_eq!(std::fs::read(&todo_path).unwrap(), before);
        drop(held);

        rewrite_live_todo_within(&r.loc(), &kept, Some(&baseline), Duration::from_millis(200))
            .unwrap();
        assert_ne!(std::fs::read(&todo_path).unwrap(), before);
        assert_eq!(
            CliGit.rebase_continue(&r.loc()).unwrap(),
            RebaseOutcome::Done
        );
    }

    #[test]
    fn range_with_merge_commit_is_refused() {
        let r = repo("merge");
        r.commit_file("f1.txt", "1\n", "c1");
        r.commit_file("f2.txt", "2\n", "c2");
        git_in(&r.dir, &["checkout", "-q", "-b", "side", "HEAD~1"]);
        r.commit_file("g1.txt", "g\n", "s1");
        git_in(&r.dir, &["checkout", "-q", "main"]);
        git_in(&r.dir, &["merge", "-q", "--no-ff", "--no-edit", "side"]);

        let c2 = r.sha_of("c2");
        let err = CliGit
            .rebase_retag(&r.loc(), &c2, &[&c2], TodoAction::Drop, &opts())
            .unwrap_err();
        assert!(err.to_string().contains("merge"), "{err}");
        // Nothing was started.
        assert!(CliGit.rebase_status(&r.loc()).unwrap().is_none());
    }

    #[test]
    fn rebase_onto_replays_only_commits_after_marked_base() {
        let r = repo("onto");
        r.commit_file("base.txt", "0\n", "c0");
        git_in(&r.dir, &["checkout", "-q", "-b", "develop"]);
        r.commit_file("d1.txt", "d\n", "d1");
        git_in(&r.dir, &["checkout", "-q", "-b", "feat"]);
        r.commit_file("fa.txt", "a\n", "f1");
        r.commit_file("fb.txt", "b\n", "f2");
        let d1 = r.sha_of("d1");
        git_in(&r.dir, &["checkout", "-q", "main"]);
        r.commit_file("m1.txt", "m\n", "m1");
        git_in(&r.dir, &["checkout", "-q", "feat"]);

        let out = CliGit.rebase_onto(&r.loc(), "main", &d1, &opts()).unwrap();
        assert_eq!(out, RebaseOutcome::Done);
        // f1/f2 replayed onto main; develop's d1 left behind.
        assert_eq!(r.subjects(), ["f2", "f1", "m1", "c0"]);
        let t = tree(&r);
        assert!(t.contains("m1.txt") && !t.contains("d1.txt"), "{t}");
    }

    #[test]
    fn allow_empty_commit_survives_rewrite() {
        let r = repo("empty");
        r.commit_file("f1.txt", "1\n", "c1");
        r.commit_file("f2.txt", "2\n", "c2");
        git_in(&r.dir, &["commit", "--allow-empty", "-q", "-m", "empty"]);
        r.commit_file("f4.txt", "4\n", "c4");

        let c2 = r.sha_of("c2");
        let out = CliGit
            .rebase_retag(&r.loc(), &c2, &[&c2], TodoAction::Drop, &opts())
            .unwrap();
        assert_eq!(out, RebaseOutcome::Done);
        // --empty=keep: the empty commit replays instead of pausing the run.
        assert_eq!(r.subjects(), ["c4", "empty", "c1"]);
        assert_eq!(touched(&r, &r.sha_of("empty")), "");
    }

    #[test]
    fn local_todo_rewrites_refuse_a_symlink_without_touching_its_target() {
        let r = linear("edit-symlink-todo");
        let c2 = r.sha_of("c2");
        assert_eq!(
            CliGit
                .rebase_retag(&r.loc(), &c2, &[&c2], TodoAction::Edit, &opts())
                .unwrap(),
            RebaseOutcome::Paused
        );
        let baseline = CliGit.rebase_status(&r.loc()).unwrap().unwrap().remaining;
        let todo_path = r.dir.join(".git/rebase-merge/git-rebase-todo");
        let outside = r.dir.join("outside-todo");
        std::fs::rename(&todo_path, &outside).unwrap();
        match thegn_core::fsperm::create_symlink(&outside, &todo_path, false) {
            Ok(()) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::Unsupported | std::io::ErrorKind::PermissionDenied
                ) =>
            {
                return;
            }
            Err(error) => panic!("create todo symlink: {error}"),
        }
        let original = std::fs::read(&outside).unwrap();
        let edited: Vec<TodoEntry> = baseline
            .iter()
            .filter(|entry| entry.subject != "c3")
            .cloned()
            .collect();

        assert!(CliGit.rewrite_pending_todo(&r.loc(), &edited).is_err());
        assert!(
            CliGit
                .rewrite_pending_todo_checked(&r.loc(), &edited, &baseline)
                .is_err()
        );
        assert_eq!(std::fs::read(&outside).unwrap(), original);
    }

    #[test]
    fn remote_todo_rewrite_refuses_a_symlink_without_touching_its_target() {
        if !flock_available() {
            return;
        }
        let r = linear("edit-remote-symlink-todo");
        let c2 = r.sha_of("c2");
        assert_eq!(
            CliGit
                .rebase_retag(&r.loc(), &c2, &[&c2], TodoAction::Edit, &opts())
                .unwrap(),
            RebaseOutcome::Paused
        );
        let remote = GitLoc::provider(vec!["env".into()], r.dir.to_string_lossy());
        let todo_path = r.dir.join(".git/rebase-merge/git-rebase-todo");
        let outside = r.dir.join("outside-remote-todo");
        std::fs::rename(&todo_path, &outside).unwrap();
        match thegn_core::fsperm::create_symlink(&outside, &todo_path, false) {
            Ok(()) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::Unsupported | std::io::ErrorKind::PermissionDenied
                ) =>
            {
                return;
            }
            Err(error) => panic!("create todo symlink: {error}"),
        }
        let original = std::fs::read(&outside).unwrap();
        let baseline = CliGit.rebase_status(&remote).unwrap().unwrap().remaining;
        let edited: Vec<TodoEntry> = baseline
            .iter()
            .filter(|entry| entry.subject != "c3")
            .cloned()
            .collect();

        assert!(CliGit.rewrite_pending_todo(&remote, &edited).is_err());
        assert!(
            CliGit
                .rewrite_pending_todo_checked(&remote, &edited, &baseline)
                .is_err()
        );
        assert_eq!(std::fs::read(&outside).unwrap(), original);
    }

    #[test]
    fn checked_provider_todo_rewrite_publishes_when_exact_bytes_match() {
        if !flock_available() {
            return;
        }
        let r = linear("checked-provider-todo-rewrite");
        let c2 = r.sha_of("c2");
        assert_eq!(
            CliGit
                .rebase_retag(&r.loc(), &c2, &[&c2], TodoAction::Edit, &opts())
                .unwrap(),
            RebaseOutcome::Paused
        );
        let provider = GitLoc::provider(vec!["env".into()], r.dir.to_string_lossy());
        let baseline = CliGit.rebase_status(&provider).unwrap().unwrap().remaining;
        let edited: Vec<TodoEntry> = baseline
            .iter()
            .filter(|entry| entry.subject != "c3")
            .cloned()
            .collect();

        CliGit
            .rewrite_pending_todo_checked(&provider, &edited, &baseline)
            .unwrap();
        assert_eq!(
            CliGit.rebase_status(&provider).unwrap().unwrap().remaining,
            edited
        );
    }

    #[test]
    fn checked_local_todo_rewrite_refuses_non_utf8_without_changing_bytes() {
        let r = linear("edit-non-utf8-todo");
        let c2 = r.sha_of("c2");
        assert_eq!(
            CliGit
                .rebase_retag(&r.loc(), &c2, &[&c2], TodoAction::Edit, &opts())
                .unwrap(),
            RebaseOutcome::Paused
        );
        let todo_path = r.dir.join(".git/rebase-merge/git-rebase-todo");
        let mut bytes = std::fs::read(&todo_path).unwrap();
        let subject = bytes
            .windows(b" c3\n".len())
            .position(|window| window == b" c3\n")
            .expect("pending c3 todo subject");
        bytes[subject + 2] = 0xff;
        std::fs::write(&todo_path, &bytes).unwrap();
        let baseline = parse_todo(&String::from_utf8_lossy(&bytes));

        let error = CliGit
            .rewrite_pending_todo_checked(&r.loc(), &baseline, &baseline)
            .expect_err("invalid UTF-8 must not be rewritten lossily");
        assert!(error.to_string().contains("not valid UTF-8"), "{error}");
        assert_eq!(std::fs::read(&todo_path).unwrap(), bytes);
    }
}
