//! The git mutation runner: every panel-initiated git WRITE flows through
//! one seam — a [`GitOp`] executed on a `spawn_blocking` thread, its
//! [`GitOpResult`] sent back over a channel with a `TerminalWaker` pulse
//! (the same shape as model hydration). One mutation runs per worktree at a
//! time, with no queue: a request while busy is rejected with a status-line
//! message (queueing compound git operations invites disaster; lazygit does
//! the same).
//!
//! History-rewriting ops record the pre-op HEAD in `undo_marks` so the
//! reflog undo planner can tell our resets from user actions, and one `z`
//! undoes a whole composite (e.g. a custom-patch split).

use anyhow::Result;

use superzej_core::rebase_todo::{TodoAction, TodoEntry};
use superzej_core::remote::GitLoc;
use superzej_svc::git::{
    BisectOps, BranchOps, CherryOps, CliGit, CommitOps, CustomOps, ForceMode, GitBackend, PatchOps,
    RebaseOps, RebaseOpts, RebaseOutcome, ResetMode, StageOps, StashOps, UndoOps,
};

/// Which pane a line-staging op targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StageTarget {
    Unstaged,
    Staged,
}

/// A git write, fully self-contained (diff text and selections are captured
/// at dispatch time so the blocking thread never touches UI state).
#[derive(Debug, Clone)]
pub enum GitOp {
    // staging
    StageLines {
        path: String,
        diff: String,
        indices: Vec<(usize, usize)>,
        target: StageTarget,
    },
    DiscardLines {
        path: String,
        diff: String,
        indices: Vec<(usize, usize)>,
    },
    StageFile {
        path: String,
        unstage: bool,
    },
    IntentToAdd {
        path: String,
    },
    DiscardFile {
        path: String,
        untracked: bool,
    },
    StageAll,
    UnstageAll,
    // commits
    Commit {
        message: String,
    },
    AmendHead,
    Reword {
        sha: String,
        message: String,
    },
    Squash {
        oldest: String,
        targets: Vec<String>,
    },
    Fixup {
        oldest: String,
        targets: Vec<String>,
    },
    Drop {
        oldest: String,
        targets: Vec<String>,
    },
    EditStop {
        sha: String,
    },
    MoveCommit {
        sha: String,
        up: bool,
    },
    AmendOldCommit {
        target: String,
    },
    Revert {
        sha: String,
    },
    ResetTo {
        sha: String,
        mode: ResetMode,
    },
    CherryPick {
        shas: Vec<String>,
    },
    // interactive rebase
    RebaseInteractive {
        base: String,
        todo: Vec<TodoEntry>,
    },
    RebaseContinue,
    RebaseSkip,
    RebaseAbort,
    RewritePendingTodo {
        todo: Vec<TodoEntry>,
        /// The pending list as the editor read it — the rewrite refuses to
        /// clobber a todo that changed on disk since.
        baseline: Vec<TodoEntry>,
    },
    // sequencer continuations (cherry-pick / merge / revert)
    CherryContinue,
    CherrySkip,
    CherryAbort,
    MergeContinue,
    MergeAbort,
    RevertContinue,
    RevertAbort,
    // branches
    Checkout {
        refname: String,
    },
    CreateBranch {
        name: String,
        base: String,
    },
    DeleteBranch {
        name: String,
        force: bool,
    },
    Merge {
        branch: String,
    },
    RebaseBranch {
        branch: String,
    },
    RebaseOnto {
        target: String,
        marked_base: String,
    },
    FastForward {
        branch: String,
        current: bool,
    },
    Push {
        force: ForceMode,
    },
    PushSetUpstream {
        remote: String,
        branch: String,
    },
    Pull,
    Fetch,
    Nuke,
    // stash
    StashPush {
        message: String,
    },
    StashApply {
        index: usize,
    },
    StashPop {
        index: usize,
    },
    StashDrop {
        index: usize,
    },
    // bisect
    BisectStart {
        bad: Option<String>,
        good: Option<String>,
    },
    BisectMark {
        term: String,
    },
    BisectSkip,
    BisectReset,
    // custom patch (patch text rendered REVERSE for the removal flows — see
    // svc::git::patch docs; forward for plain applies)
    PatchApply {
        patch: String,
        reverse: bool,
    },
    PatchRemoveFromCommit {
        sha: String,
        patch: String,
    },
    PatchSplit {
        sha: String,
        patch: String,
        message: String,
    },
    PatchToIndex {
        sha: String,
        patch: String,
    },
    // reflog undo
    UndoPlan {
        redo: bool,
    },
    UndoApply {
        plan: superzej_core::reflog::UndoPlan,
        autostash: bool,
    },
    // custom commands ([[git_commands]] popup/none output)
    Custom {
        command: String,
        capture: bool,
    },
}

impl GitOp {
    /// The status-line label while the op runs.
    pub fn label(&self) -> &'static str {
        match self {
            GitOp::StageLines {
                target: StageTarget::Unstaged,
                ..
            } => "staging lines",
            GitOp::StageLines { .. } => "unstaging lines",
            GitOp::DiscardLines { .. } => "discarding lines",
            GitOp::StageFile { unstage: false, .. } => "staging",
            GitOp::StageFile { .. } => "unstaging",
            GitOp::IntentToAdd { .. } => "tracking",
            GitOp::DiscardFile { .. } => "discarding",
            GitOp::StageAll => "staging all",
            GitOp::UnstageAll => "unstaging all",
            GitOp::Commit { .. } => "committing",
            GitOp::AmendHead => "amending",
            GitOp::Reword { .. } => "rewording",
            GitOp::Squash { .. } => "squashing",
            GitOp::Fixup { .. } => "fixing up",
            GitOp::Drop { .. } => "dropping",
            GitOp::EditStop { .. } => "rebasing to edit",
            GitOp::MoveCommit { .. } => "moving commit",
            GitOp::AmendOldCommit { .. } => "amending commit",
            GitOp::Revert { .. } => "reverting",
            GitOp::ResetTo { .. } => "resetting",
            GitOp::CherryPick { .. } => "cherry-picking",
            GitOp::RebaseInteractive { .. } => "rebasing",
            GitOp::RebaseContinue => "continuing rebase",
            GitOp::RebaseSkip => "skipping",
            GitOp::RebaseAbort => "aborting rebase",
            GitOp::RewritePendingTodo { .. } => "editing todo",
            GitOp::CherryContinue => "continuing cherry-pick",
            GitOp::CherrySkip => "skipping",
            GitOp::CherryAbort => "aborting cherry-pick",
            GitOp::MergeContinue => "continuing merge",
            GitOp::MergeAbort => "aborting merge",
            GitOp::RevertContinue => "continuing revert",
            GitOp::RevertAbort => "aborting revert",
            GitOp::Checkout { .. } => "checking out",
            GitOp::CreateBranch { .. } => "branching",
            GitOp::DeleteBranch { .. } => "deleting branch",
            GitOp::Merge { .. } => "merging",
            GitOp::RebaseBranch { .. } | GitOp::RebaseOnto { .. } => "rebasing",
            GitOp::FastForward { .. } => "fast-forwarding",
            GitOp::Push { .. } => "pushing",
            GitOp::PushSetUpstream { .. } => "pushing (set upstream)",
            GitOp::Pull => "pulling",
            GitOp::Fetch => "fetching",
            GitOp::Nuke => "nuking working tree",
            GitOp::StashPush { .. } => "stashing",
            GitOp::StashApply { .. } => "applying stash",
            GitOp::StashPop { .. } => "popping stash",
            GitOp::StashDrop { .. } => "dropping stash",
            GitOp::BisectStart { .. } => "starting bisect",
            GitOp::BisectMark { .. } => "bisecting",
            GitOp::BisectSkip => "skipping",
            GitOp::BisectReset => "ending bisect",
            GitOp::PatchApply { .. } => "applying patch",
            GitOp::PatchRemoveFromCommit { .. } => "removing patch",
            GitOp::PatchSplit { .. } => "splitting patch",
            GitOp::PatchToIndex { .. } => "moving patch to index",
            GitOp::UndoPlan { redo: false } => "planning undo",
            GitOp::UndoPlan { .. } => "planning redo",
            GitOp::UndoApply { .. } => "undoing",
            GitOp::Custom { .. } => "running command",
        }
    }

    /// Whether the op rewrites history (records an undo mark + may need the
    /// gpg override).
    fn rewrites_history(&self) -> bool {
        matches!(
            self,
            GitOp::Reword { .. }
                | GitOp::Squash { .. }
                | GitOp::Fixup { .. }
                | GitOp::Drop { .. }
                | GitOp::MoveCommit { .. }
                | GitOp::AmendOldCommit { .. }
                | GitOp::AmendHead
                | GitOp::RebaseInteractive { .. }
                | GitOp::RebaseBranch { .. }
                | GitOp::RebaseOnto { .. }
                | GitOp::ResetTo { .. }
                | GitOp::PatchRemoveFromCommit { .. }
                | GitOp::PatchSplit { .. }
                | GitOp::PatchToIndex { .. }
        )
    }

    /// Whether a successful run should refresh the PR caches too.
    pub fn touches_remote(&self) -> bool {
        matches!(
            self,
            GitOp::Push { .. }
                | GitOp::PushSetUpstream { .. }
                | GitOp::Pull
                | GitOp::Fetch
                | GitOp::FastForward { .. }
        )
    }
}

/// What landed back on the loop.
#[derive(Debug)]
pub enum GitOpResult {
    /// Success; optional human status note.
    Ok(Option<String>),
    /// The op stopped on a conflict or deliberate pause — the conflict
    /// banner / rebase view takes over (hydration re-detects the state).
    Stopped(RebaseOutcome),
    /// Bisect found the culprit.
    Culprit(String),
    /// A computed undo/redo plan for the confirm dialog.
    Plan {
        plan: superzej_core::reflog::UndoPlan,
        redo: bool,
    },
    /// Captured custom-command output for the popup.
    Output(String),
    Err(String),
    /// Push failed because the branch has no upstream. The caller can offer
    /// to run `push -u origin <branch>`.
    NoUpstream {
        branch: String,
    },
}

fn selection_of(indices: &[(usize, usize)]) -> superzej_core::patch::Selection {
    let mut sel = superzej_core::patch::Selection::default();
    for &(h, l) in indices {
        sel.insert(h, l);
    }
    sel
}

/// Record the pre-op HEAD as an undo mark (best-effort — the undo planner
/// degrades to treating our reset as a user action when the DB is away).
fn record_mark(loc: &GitLoc) {
    if let Some(head) = loc.git_out(&["rev-parse", "HEAD"])
        && let Ok(db) = superzej_core::db::Db::open()
    {
        let _ = db.add_undo_mark(&loc.path(), &head);
    }
}

/// Run one op to completion on the current (blocking) thread.
pub fn execute(op: GitOp, loc: &GitLoc, override_gpg: bool) -> GitOpResult {
    let opts = RebaseOpts { override_gpg };
    if op.rewrites_history() {
        record_mark(loc);
    }
    let g = CliGit;
    let done = |r: Result<()>| match r {
        Ok(()) => GitOpResult::Ok(None),
        Err(e) => GitOpResult::Err(first_line(&e)),
    };
    let stopped = |r: Result<RebaseOutcome>| match r {
        Ok(RebaseOutcome::Done) => GitOpResult::Ok(None),
        Ok(out) => GitOpResult::Stopped(out),
        Err(e) => GitOpResult::Err(first_line(&e)),
    };
    match op {
        GitOp::StageLines {
            path,
            diff,
            indices,
            target,
        } => {
            let n = indices.len();
            let sel = selection_of(&indices);
            let res = match target {
                StageTarget::Unstaged => g.stage_lines(loc, &diff, &sel),
                StageTarget::Staged => g.unstage_lines(loc, &diff, &sel),
            };
            match res {
                Ok(()) => {
                    let verb = match target {
                        StageTarget::Unstaged => "staged",
                        StageTarget::Staged => "unstaged",
                    };
                    GitOpResult::Ok(Some(format!("{verb} {n} line(s) in {path}")))
                }
                Err(e) => GitOpResult::Err(first_line(&e)),
            }
        }
        GitOp::DiscardLines {
            path,
            diff,
            indices,
        } => {
            let n = indices.len();
            match g.discard_lines(loc, &diff, &selection_of(&indices)) {
                Ok(()) => GitOpResult::Ok(Some(format!("discarded {n} line(s) in {path}"))),
                Err(e) => GitOpResult::Err(first_line(&e)),
            }
        }
        GitOp::StageFile { path, unstage } => done(if unstage {
            g.unstage(loc, &path)
        } else {
            g.stage(loc, &path)
        }),
        GitOp::IntentToAdd { path } => done(g.intent_to_add(loc, &path)),
        GitOp::DiscardFile { path, untracked } => done(g.discard_file(loc, &path, untracked)),
        GitOp::StageAll => done(g.stage_all(loc)),
        GitOp::UnstageAll => done(g.unstage_all(loc)),
        GitOp::Commit { message } => done(g.commit(loc, &message, false)),
        GitOp::AmendHead => done(g.commit_amend(loc, false, override_gpg)),
        GitOp::Reword { sha, message } => done(g.reword(loc, &sha, &message, &opts)),
        GitOp::Squash { oldest, targets } => {
            let t: Vec<&str> = targets.iter().map(String::as_str).collect();
            stopped(g.rebase_retag(loc, &oldest, &t, TodoAction::Squash, &opts))
        }
        GitOp::Fixup { oldest, targets } => {
            let t: Vec<&str> = targets.iter().map(String::as_str).collect();
            stopped(g.rebase_retag(loc, &oldest, &t, TodoAction::Fixup, &opts))
        }
        GitOp::Drop { oldest, targets } => {
            let t: Vec<&str> = targets.iter().map(String::as_str).collect();
            stopped(g.rebase_retag(loc, &oldest, &t, TodoAction::Drop, &opts))
        }
        GitOp::EditStop { sha } => {
            stopped(g.rebase_retag(loc, &sha, &[&sha], TodoAction::Edit, &opts))
        }
        GitOp::MoveCommit { sha, up } => stopped(g.rebase_move(loc, &sha, up, &opts)),
        GitOp::AmendOldCommit { target } => stopped(g.amend_old_commit(loc, &target, &opts)),
        GitOp::Revert { sha } => done(g.revert(loc, &sha, None)),
        GitOp::ResetTo { sha, mode } => done(g.reset_to(loc, &sha, mode)),
        GitOp::CherryPick { shas } => {
            let s: Vec<&str> = shas.iter().map(String::as_str).collect();
            done(g.cherry_pick(loc, &s, None, override_gpg))
        }
        GitOp::RebaseInteractive { base, todo } => {
            stopped(g.rebase_interactive(loc, &base, &todo, &opts))
        }
        GitOp::RebaseContinue => stopped(g.rebase_continue(loc)),
        GitOp::RebaseSkip => stopped(g.rebase_skip(loc)),
        GitOp::RebaseAbort => done(g.rebase_abort(loc)),
        GitOp::RewritePendingTodo { todo, baseline } => {
            done(g.rewrite_pending_todo_checked(loc, &todo, &baseline))
        }
        GitOp::CherryContinue => done(g.cherry_continue(loc)),
        GitOp::CherrySkip => done(g.cherry_skip(loc)),
        GitOp::CherryAbort => done(g.cherry_abort(loc)),
        GitOp::MergeContinue => done(g.merge_continue(loc)),
        GitOp::MergeAbort => done(g.merge_abort(loc)),
        GitOp::RevertContinue => done(g.revert_continue(loc)),
        GitOp::RevertAbort => done(g.revert_abort(loc)),
        GitOp::Checkout { refname } => done(g.checkout(loc, &refname)),
        GitOp::CreateBranch { name, base } => done(g.create_branch(loc, &name, &base)),
        GitOp::DeleteBranch { name, force } => done(g.delete_branch(loc, &name, force)),
        GitOp::Merge { branch } => done(g.merge(loc, &branch)),
        GitOp::RebaseBranch { branch } => stopped(g.rebase_branch(loc, &branch, &opts)),
        GitOp::RebaseOnto {
            target,
            marked_base,
        } => stopped(g.rebase_onto(loc, &target, &marked_base, &opts)),
        GitOp::FastForward { branch, current } => {
            done(g.fast_forward(loc, &branch, current, "origin"))
        }
        GitOp::PushSetUpstream { remote, branch } => {
            done(g.push_set_upstream(loc, &remote, &branch))
        }
        GitOp::Push { force } => {
            match g.push(loc, force) {
                Ok(()) => GitOpResult::Ok(None),
                Err(e) => {
                    let msg = format!("{e}");
                    // `git push` with no upstream prints "has no upstream branch"
                    // or "no configured push destination". Offer to set it.
                    if msg.contains("has no upstream branch")
                        || msg.contains("no configured push destination")
                        || msg.contains("set-upstream")
                    {
                        let branch = loc
                            .git_out(&["rev-parse", "--abbrev-ref", "HEAD"])
                            .unwrap_or_default();
                        GitOpResult::NoUpstream { branch }
                    } else {
                        GitOpResult::Err(first_line_str(&msg))
                    }
                }
            }
        }
        GitOp::Pull => done(g.pull(loc)),
        GitOp::Fetch => done(g.fetch(loc)),
        GitOp::Nuke => done(g.nuke_working_tree(loc)),
        GitOp::StashPush { message } => done(g.stash_push(loc, &message, true)),
        GitOp::StashApply { index } => done(g.stash_apply(loc, index)),
        GitOp::StashPop { index } => done(g.stash_pop(loc, index)),
        GitOp::StashDrop { index } => done(g.stash_drop(loc, index)),
        GitOp::BisectStart { bad, good } => {
            match g.bisect_start(loc, bad.as_deref(), good.as_deref()) {
                Ok(Some(culprit)) => GitOpResult::Culprit(culprit),
                Ok(None) => GitOpResult::Ok(None),
                Err(e) => GitOpResult::Err(first_line(&e)),
            }
        }
        GitOp::BisectMark { term } => match g.bisect_mark(loc, &term, None) {
            Ok(Some(culprit)) => GitOpResult::Culprit(culprit),
            Ok(None) => GitOpResult::Ok(None),
            Err(e) => GitOpResult::Err(first_line(&e)),
        },
        GitOp::BisectSkip => match g.bisect_skip(loc) {
            Ok(Some(culprit)) => GitOpResult::Culprit(culprit),
            Ok(None) => GitOpResult::Ok(None),
            Err(e) => GitOpResult::Err(first_line(&e)),
        },
        GitOp::BisectReset => done(g.bisect_reset(loc)),
        GitOp::PatchApply { patch, reverse } => done(g.apply_patch(loc, &patch, reverse, false)),
        GitOp::PatchRemoveFromCommit { sha, patch } => {
            stopped(g.remove_patch_from_commit(loc, &sha, &patch, &opts))
        }
        GitOp::PatchSplit {
            sha,
            patch,
            message,
        } => stopped(g.split_patch_into_commit(loc, &sha, &patch, &message, &opts)),
        GitOp::PatchToIndex { sha, patch } => {
            stopped(g.move_patch_to_index(loc, &sha, &patch, &opts))
        }
        GitOp::UndoPlan { redo } => {
            let marks =
                match superzej_core::db::Db::open().and_then(|db| db.undo_marks(&loc.path())) {
                    Ok(m) => superzej_core::reflog::OurMarks::new(m),
                    Err(_) => superzej_core::reflog::OurMarks::default(),
                };
            let plan = if redo {
                g.redo_plan(loc, &marks)
            } else {
                g.undo_plan(loc, &marks)
            };
            match plan {
                Ok(plan) => GitOpResult::Plan { plan, redo },
                Err(e) => GitOpResult::Err(first_line(&e)),
            }
        }
        GitOp::UndoApply { plan, autostash } => match g.undo_apply(loc, &plan, autostash) {
            Ok(mark) => {
                if let Some(sha) = mark
                    && let Ok(db) = superzej_core::db::Db::open()
                {
                    let _ = db.add_undo_mark(&loc.path(), &sha);
                }
                GitOpResult::Ok(Some("undone".into()))
            }
            Err(e) => GitOpResult::Err(first_line(&e)),
        },
        GitOp::Custom { command, capture } => match g.run_custom(loc, &command) {
            Ok(out) if capture => GitOpResult::Output(out),
            Ok(_) => GitOpResult::Ok(None),
            Err(e) => GitOpResult::Err(first_line(&e)),
        },
    }
}

fn first_line(e: &anyhow::Error) -> String {
    first_line_str(&e.to_string())
}

fn first_line_str(s: &str) -> String {
    s.lines().next().unwrap_or("git error").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_repo(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("sz-gitmut-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let git = |args: &[&str]| {
            let ok = std::process::Command::new("git")
                .args(args)
                .current_dir(&dir)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@e")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@e")
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);
            assert!(ok, "git {args:?}");
        };
        git(&["init", "-q", "-b", "main"]);
        git(&["config", "user.name", "t"]);
        git(&["config", "user.email", "t@e"]);
        git(&["config", "commit.gpgsign", "false"]);
        std::fs::write(dir.join("f.txt"), "one\n").unwrap();
        git(&["add", "f.txt"]);
        git(&["commit", "-q", "-m", "c0"]);
        dir
    }

    #[test]
    fn execute_runs_a_commit_and_reports_errors_one_line() {
        let dir = tmp_repo("exec");
        let loc = GitLoc::Local(dir.clone());
        std::fs::write(dir.join("f.txt"), "two\n").unwrap();
        match execute(GitOp::StageAll, &loc, false) {
            GitOpResult::Ok(_) => {}
            other => panic!("{other:?}"),
        }
        match execute(
            GitOp::Commit {
                message: "msg line\nbody".into(),
            },
            &loc,
            false,
        ) {
            GitOpResult::Ok(_) => {}
            other => panic!("{other:?}"),
        }
        // Error path: deleting the current branch fails with a one-line msg.
        match execute(
            GitOp::DeleteBranch {
                name: "main".into(),
                force: true,
            },
            &loc,
            false,
        ) {
            GitOpResult::Err(msg) => assert!(!msg.contains('\n')),
            other => panic!("{other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn labels_exist_for_every_op_shape() {
        assert_eq!(GitOp::StageAll.label(), "staging all");
        assert!(
            GitOp::Push {
                force: ForceMode::None
            }
            .touches_remote()
        );
        assert!(
            !GitOp::StashPush {
                message: String::new()
            }
            .touches_remote()
        );
        assert!(GitOp::AmendHead.rewrites_history());
        assert!(
            !GitOp::Commit {
                message: String::new()
            }
            .rewrites_history()
        );
    }
}
