//! The git mutation runner: every panel-initiated git WRITE flows through
//! one seam — a [`GitOp`] executed on a `spawn_blocking` thread, its
//! [`GitOpResult`] sent back over a channel with a `TerminalWaker` pulse
//! (the same shape as model hydration). One mutation runs per worktree at a
//! time, with no queue: a request while busy is rejected with a status-line
//! message (queueing compound git operations invites disaster; lazygit does
//! the same).
//!
//! History-rewriting ops record the pre-op HEAD in `undo_marks`, keyed by the
//! host-side worktree path, so the reflog undo planner can tell our resets from
//! user actions. An unreadable mark set refuses undo/redo planning rather than
//! treating the provenance as empty, and one `z` undoes a whole composite
//! (e.g. a custom-patch split).

use anyhow::Result;

use thegn_core::git_run::GitTextError;
use thegn_core::rebase_todo::{TodoAction, TodoEntry};
use thegn_core::remote::GitLoc;
use thegn_core::store::WorktreeAuxStore;
use thegn_svc::git::{
    BisectOps, BranchOps, CUSTOM_COMMAND_TIMEOUT_SECS, CUSTOM_OUTCOME_CANCELLED,
    CUSTOM_OUTCOME_CLEANUP, CUSTOM_OUTCOME_EXITED, CUSTOM_OUTCOME_READ, CUSTOM_OUTCOME_SPAWN,
    CUSTOM_OUTCOME_TIMEOUT, CUSTOM_OUTCOME_WAIT, CherryOps, CliGit, CommitOps, CustomOps,
    ForceMode, GitBackend, PatchOps, RebaseOps, RebaseOpts, RebaseOutcome, ResetMode, StageOps,
    StashOps, UndoOps,
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
    /// First line-stage operation on an untracked file. Intent-to-add and
    /// validation against the read-only synthesized diff happen atomically on
    /// the mutation worker before selected lines enter the index.
    StageUntrackedLines {
        path: String,
        synth_diff: String,
        indices: Vec<(usize, usize)>,
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
    DiscardFile {
        path: String,
        untracked: bool,
    },
    /// Batch discard for the rollback window (item 604): each `(path,
    /// untracked)` is restored (tracked → `checkout --`) or deleted (untracked
    /// → `clean -f`). One enqueue keeps to the one-mutation-per-worktree rule.
    DiscardFiles {
        paths: Vec<(String, bool)>,
    },
    StageAll,
    UnstageAll,
    // commits
    Commit {
        message: String,
        /// Skip pre-commit / commit-msg hooks (`--no-verify`) — item 329.
        no_verify: bool,
        /// Override commit signing: `Some(true)`→`--gpg-sign`,
        /// `Some(false)`→`--no-gpg-sign`, `None`→inherit config — item 328.
        sign: Option<bool>,
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
        sha: String,
    },
    StashPop {
        sha: String,
    },
    StashDrop {
        sha: String,
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
        plan: thegn_core::reflog::UndoPlan,
        autostash: bool,
    },
    // custom commands ([[git_commands]] popup/none output)
    Custom {
        command: String,
        capture: bool,
    },
    CustomTemplate {
        command: thegn_core::custom_cmd::ExpandedCommand,
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
            GitOp::StageUntrackedLines { .. } => "staging lines",
            GitOp::DiscardLines { .. } => "discarding lines",
            GitOp::StageFile { unstage: false, .. } => "staging",
            GitOp::StageFile { .. } => "unstaging",
            GitOp::DiscardFile { .. } => "discarding",
            GitOp::DiscardFiles { .. } => "rolling back",
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
            GitOp::Custom { .. } | GitOp::CustomTemplate { .. } => "running command",
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
        plan: thegn_core::reflog::UndoPlan,
        redo: bool,
    },
    /// Captured custom-command output for the popup.
    Output(String),
    /// A commit was rejected by a pre-commit / commit-msg hook (item 329); the
    /// hook's combined output is surfaced in the output popup, not just the
    /// status line, so the user can see *why* it was blocked.
    HookFailed {
        output: String,
    },
    Err(String),
    /// Push failed because the branch has no upstream. The caller can offer
    /// to run `push -u origin <branch>`.
    NoUpstream {
        branch: String,
    },
}

fn selection_of(indices: &[(usize, usize)]) -> thegn_core::patch::Selection {
    let mut sel = thegn_core::patch::Selection::default();
    for &(h, l) in indices {
        sel.insert(h, l);
    }
    sel
}

fn touches_index(op: &GitOp) -> bool {
    matches!(
        op,
        GitOp::StageLines { .. }
            | GitOp::StageUntrackedLines { .. }
            | GitOp::StageFile { .. }
            | GitOp::StageAll
            | GitOp::UnstageAll
            | GitOp::Commit { .. }
            | GitOp::AmendHead
    )
}

/// Host-side guard for every line-level staging entry point. The same core
/// validator used by the patch transform rejects a mode-160000 entry before a
/// [`GitOp`] is queued, so the mutation worker never attempts `git apply` for a
/// submodule pointer.
pub(crate) fn validate_line_selection(
    diff: &str,
    indices: &[(usize, usize)],
) -> std::result::Result<(), String> {
    let selection = selection_of(indices);
    for file in thegn_core::patch::parse_patch(diff) {
        selection
            .validate_for_file(&file)
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn add_mark(
    db: &thegn_core::db::Db,
    worktree_key: &str,
    sha: &str,
) -> std::result::Result<(), String> {
    db.add_undo_mark(worktree_key, sha)
        .map_err(|error| format!("could not write undo mark: {error:#}"))
}

/// Record the pre-op HEAD as an undo mark. The git mutation still runs when
/// durable provenance cannot be recorded, but its result reports the failure.
fn record_mark(
    loc: &GitLoc,
    worktree_key: &str,
    open_db: &impl Fn() -> Result<thegn_core::db::Db>,
) -> std::result::Result<(), String> {
    let head = loc
        .git_text(&["rev-parse", "HEAD"])
        .map_err(|error| format!("could not read HEAD for undo mark: {error}"))?;
    if head.is_empty() {
        return Err("could not read HEAD for undo mark: Git returned an empty object id".into());
    }
    let db = open_db().map_err(|error| format!("could not open undo mark database: {error:#}"))?;
    add_mark(&db, worktree_key, &head)
}

fn marks_from(
    read: Result<Vec<String>>,
) -> std::result::Result<thegn_core::reflog::OurMarks, String> {
    read.map(thegn_core::reflog::OurMarks::new)
        .map_err(|error| format!("undo provenance unavailable: {error:#}"))
}

fn with_provenance_note(result: GitOpResult, label: &str, failure: Option<String>) -> GitOpResult {
    let Some(failure) = failure else {
        return result;
    };
    match result {
        GitOpResult::Ok(note) => {
            let provenance = format!("undo provenance not recorded: {failure}");
            GitOpResult::Ok(Some(match note {
                Some(note) => format!("{note}; {provenance}"),
                None => format!("{label} done; {provenance}"),
            }))
        }
        other => other,
    }
}

/// Run one op to completion on the current (blocking) thread.
pub fn execute_at(op: GitOp, loc: &GitLoc, worktree_key: &str, override_gpg: bool) -> GitOpResult {
    execute_at_with_db(
        op,
        loc,
        worktree_key,
        override_gpg,
        thegn_core::db::Db::open,
    )
}

/// Resolve the worktree placement before allowing a panel git mutation to run.
pub(crate) fn execute_for_worktree(
    op: GitOp,
    worktree: &std::path::Path,
    override_gpg: bool,
) -> GitOpResult {
    let loc = match GitLoc::for_worktree_op(worktree, op.label()) {
        Ok(loc) => loc,
        Err(error) => return GitOpResult::Err(error),
    };
    execute_at(
        op,
        &loc,
        &GitLoc::worktree_cache_key(worktree),
        override_gpg,
    )
}

/// Resolve the worktree placement before staging paths through the control API.
pub(crate) fn control_stage(worktree: &std::path::Path, paths: &[String]) -> anyhow::Result<()> {
    let loc = GitLoc::for_worktree_op(worktree, "git stage").map_err(anyhow::Error::msg)?;
    for path in paths {
        CliGit.stage(&loc, path)?;
    }
    Ok(())
}

/// Resolve the worktree placement before committing through the control API.
pub(crate) fn control_commit(worktree: &std::path::Path, message: &str) -> anyhow::Result<String> {
    let loc = GitLoc::for_worktree_op(worktree, "git commit").map_err(anyhow::Error::msg)?;
    CliGit.commit(&loc, message, false, None)?;
    loc.git_out(&["rev-parse", "HEAD"])
        .ok_or_else(|| anyhow::anyhow!("rev-parse HEAD failed after the commit"))
}

fn execute_at_with_db(
    op: GitOp,
    loc: &GitLoc,
    worktree_key: &str,
    override_gpg: bool,
    open_db: impl Fn() -> Result<thegn_core::db::Db>,
) -> GitOpResult {
    let label = op.label();
    let jj_note = touches_index(&op)
        && matches!(loc, GitLoc::Local(path) if thegn_core::jj::is_colocated(path));
    let opts = RebaseOpts { override_gpg };
    let provenance_failure = op
        .rewrites_history()
        .then(|| record_mark(loc, worktree_key, &open_db))
        .and_then(std::result::Result::err);
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
    let result = match op {
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
        GitOp::StageUntrackedLines {
            path,
            synth_diff,
            indices,
        } => {
            let res = (|| -> Result<()> {
                g.intent_to_add(loc, &path)?;
                let current = g.unstaged_diff(loc, &path)?;
                let body = |diff: &str| {
                    crate::panel::staging::build(&path, diff)
                        .lines
                        .into_iter()
                        .filter(|line| !line.is_header)
                        .map(|line| (line.kind, line.text))
                        .collect::<Vec<_>>()
                };
                if body(&current) != body(&synth_diff) {
                    anyhow::bail!("file changed while staging; review and stage again");
                }
                g.stage_lines(loc, &current, &selection_of(&indices))
            })();
            match res {
                Ok(()) => {
                    GitOpResult::Ok(Some(format!("staged {} line(s) in {path}", indices.len())))
                }
                Err(error) => GitOpResult::Err(first_line(&error)),
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
        GitOp::DiscardFile { path, untracked } => done(g.discard_file(loc, &path, untracked)),
        GitOp::DiscardFiles { paths } => {
            let n = paths.len();
            let mut err = None;
            for (path, untracked) in &paths {
                if let Err(e) = g.discard_file(loc, path, *untracked) {
                    err = Some(first_line(&e));
                    break;
                }
            }
            match err {
                None => GitOpResult::Ok(Some(format!("rolled back {n} file(s)"))),
                Some(msg) => GitOpResult::Err(msg),
            }
        }
        GitOp::StageAll => done(g.stage_all(loc)),
        GitOp::UnstageAll => done(g.unstage_all(loc)),
        GitOp::Commit {
            message,
            no_verify,
            sign,
        } => match g.commit(loc, &message, no_verify, sign) {
            Ok(()) => GitOpResult::Ok(None),
            Err(e) => {
                // `run_stdin` folds the hook's stdout+stderr into the error, so
                // a rejected commit carries the hook's own message (item 329) —
                // route that to the popup. Git's own terse refusals ("nothing
                // to commit", which is itself multi-line) stay on the status
                // line; a newline count can't tell the two apart, so we match
                // the known terse messages and treat everything else as hook
                // output.
                let full = format!("{e}");
                let detail = full.split_once("failed: ").map_or("", |(_, d)| d).trim();
                let terse = detail.is_empty()
                    || detail.contains("nothing to commit")
                    || detail.contains("nothing added to commit")
                    || detail.contains("no changes added to commit");
                if terse {
                    GitOpResult::Err(first_line_str(&full))
                } else {
                    GitOpResult::HookFailed {
                        output: sanitize_and_redact_popup(&full),
                    }
                }
            }
        },
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
                        no_upstream_result(
                            loc.git_text(&["rev-parse", "--abbrev-ref", "HEAD"]),
                            &msg,
                        )
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
        GitOp::StashApply { sha } => done(g.stash_apply(loc, &sha)),
        GitOp::StashPop { sha } => done(g.stash_pop(loc, &sha)),
        GitOp::StashDrop { sha } => done(g.stash_drop(loc, &sha)),
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
            let marks = match marks_from(open_db().and_then(|db| db.undo_marks(worktree_key))) {
                Ok(marks) => marks,
                Err(message) => return GitOpResult::Err(message),
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
                let write_failure = mark.and_then(|sha| {
                    open_db()
                        .map_err(|error| format!("could not open undo mark database: {error:#}"))
                        .and_then(|db| add_mark(&db, worktree_key, &sha))
                        .err()
                });
                with_provenance_note(
                    GitOpResult::Ok(Some("undone".into())),
                    "undo",
                    write_failure,
                )
            }
            Err(e) => GitOpResult::Err(first_line(&e)),
        },
        GitOp::Custom { command, capture } => {
            let _in_flight = crate::quit_cancel::custom_git().enter();
            custom_command_result(
                g.run_custom(
                    loc,
                    &command,
                    capture,
                    crate::quit_cancel::custom_git().cancel_flag(),
                ),
                capture,
                false,
                CUSTOM_COMMAND_TIMEOUT_SECS,
            )
        }
        GitOp::CustomTemplate { command, capture } => {
            let _in_flight = crate::quit_cancel::custom_git().enter();
            let timeout_secs = command.timeout().as_secs();
            custom_command_result(
                g.run_custom_template(
                    loc,
                    &command,
                    capture,
                    crate::quit_cancel::custom_git().cancel_flag(),
                ),
                capture,
                true,
                timeout_secs,
            )
        }
    };
    let result = with_provenance_note(result, label, provenance_failure);
    match result {
        GitOpResult::Ok(note) if jj_note => GitOpResult::Ok(Some(format!(
            "{} — jj ignores the git index",
            note.unwrap_or_else(|| format!("{label} done"))
        ))),
        other => other,
    }
}

#[cfg(test)]
fn execute(op: GitOp, loc: &GitLoc, override_gpg: bool) -> GitOpResult {
    execute_at(op, loc, &loc.path(), override_gpg)
}

/// Make popup text safe for terminal rendering and redact secrets after
/// sanitization, so terminal controls cannot split a secret token.
fn sanitize_and_redact_popup(output: &str) -> String {
    let sanitized = thegn_svc::lsp::sanitize_for_terminal(output);
    thegn_core::ci_log::redact(&sanitized)
}

fn custom_command_result(
    outcome: (
        &'static str,
        Option<std::process::ExitStatus>,
        String,
        bool,
        String,
    ),
    capture: bool,
    redact_failure: bool,
    timeout_secs: u64,
) -> GitOpResult {
    let (kind, status, stdout, stdout_truncated, stderr) = outcome;
    match kind {
        CUSTOM_OUTCOME_EXITED if status.as_ref().is_some_and(|status| status.success()) => {
            if capture {
                let mut output = sanitize_and_redact_popup(&stdout);
                if stdout_truncated {
                    output.push_str("\n… output truncated at 1 MiB");
                }
                GitOpResult::Output(output)
            } else {
                GitOpResult::Ok(None)
            }
        }
        CUSTOM_OUTCOME_EXITED => {
            let status = status.map_or_else(|| "unknown status".to_string(), |s| s.to_string());
            if redact_failure {
                GitOpResult::Err(format!("custom command failed ({status})"))
            } else {
                let safe_output = sanitize_and_redact_popup(&stderr);
                let tail: Vec<String> = safe_output
                    .lines()
                    .filter(|line| !line.trim().is_empty())
                    .map(|line| line.chars().take(200).collect())
                    .collect();
                let tail: Vec<&str> = tail.iter().rev().take(4).map(String::as_str).collect();
                let tail: Vec<&str> = tail.into_iter().rev().collect();
                GitOpResult::Err(if tail.is_empty() {
                    format!("command failed ({status})")
                } else {
                    format!("command failed: {}", tail.join(" · "))
                })
            }
        }
        CUSTOM_OUTCOME_TIMEOUT => GitOpResult::Err(format!(
            "custom command timed out after {timeout_secs}s and was killed"
        )),
        CUSTOM_OUTCOME_CANCELLED => {
            GitOpResult::Err("custom command cancelled (thegn is quitting)".into())
        }
        CUSTOM_OUTCOME_SPAWN => GitOpResult::Err("could not start custom command".into()),
        CUSTOM_OUTCOME_WAIT => GitOpResult::Err("custom command could not be reaped".into()),
        CUSTOM_OUTCOME_READ => GitOpResult::Err("could not read custom command output".into()),
        CUSTOM_OUTCOME_CLEANUP => {
            GitOpResult::Err("custom command cleanup did not complete".into())
        }
        _ => GitOpResult::Err("custom command returned an unknown outcome".into()),
    }
}

fn first_line(e: &anyhow::Error) -> String {
    first_line_str(&e.to_string())
}

fn first_line_str(s: &str) -> String {
    s.lines().next().unwrap_or("git error").to_string()
}

fn no_upstream_result(
    branch: std::result::Result<String, GitTextError>,
    push_error: &str,
) -> GitOpResult {
    match branch {
        Ok(branch) if !branch.is_empty() && branch != "HEAD" => GitOpResult::NoUpstream { branch },
        Ok(_) | Err(_) => GitOpResult::Err(first_line_str(push_error)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn undo_mark_head_read_failure_carries_git_reason() {
        let non_repo = tempfile::tempdir().expect("non-repository dir");
        let db_dir = tempfile::tempdir().expect("database dir");
        let db_path = db_dir.path().join("marks.db");
        let result = record_mark(
            &GitLoc::Local(non_repo.path().to_path_buf()),
            "key",
            &|| thegn_core::db::Db::open_at(&db_path),
        );

        assert!(matches!(result, Err(message)
            if message.contains("could not read HEAD for undo mark")
                && message.contains("status Some(128)")));
    }

    #[test]
    fn no_upstream_needs_a_named_branch() {
        assert!(matches!(
            no_upstream_result(Ok("feat".into()), "fatal: no upstream\nmore detail"),
            GitOpResult::NoUpstream { branch } if branch == "feat"
        ));
        for branch in [Ok("HEAD".to_string()), Ok(String::new())] {
            assert!(matches!(
                no_upstream_result(branch, "fatal: no upstream\nmore detail"),
                GitOpResult::Err(message) if message == "fatal: no upstream"
            ));
        }
        assert!(matches!(
            no_upstream_result(Err(GitTextError::InvalidUtf8), "fatal: no upstream\nmore detail"),
            GitOpResult::Err(message) if message == "fatal: no upstream"
        ));
    }

    #[test]
    fn undo_marks_are_keyed_by_host_worktree_not_sandbox_path() {
        let dir = tmp_repo("undo-mark-authority");
        let loc = GitLoc::Local(dir.clone());
        let initial = git_out(&dir, &["rev-parse", "HEAD"]);
        std::fs::write(dir.join("f.txt"), "two\n").unwrap();
        git_out(&dir, &["add", "f.txt"]);
        git_out(&dir, &["commit", "-q", "-m", "c1"]);
        git_out(&dir, &["reset", "--hard", &initial]);

        let temp = tempfile::tempdir().unwrap();
        let db_path = temp.path().join("marks.db");
        let db = thegn_core::db::Db::open_at(&db_path).unwrap();
        let worktree_key = GitLoc::worktree_cache_key(std::path::Path::new("/host/worktree"));
        assert_ne!(worktree_key, loc.path());
        add_mark(&db, &worktree_key, &initial).unwrap();

        let result = execute_at_with_db(
            GitOp::UndoPlan { redo: false },
            &loc,
            &worktree_key,
            false,
            || thegn_core::db::Db::open_at(&db_path),
        );

        assert!(matches!(
            result,
            GitOpResult::Plan {
                plan: thegn_core::reflog::UndoPlan::Nothing,
                redo: false
            }
        ));
    }

    #[test]
    fn unreadable_undo_marks_refuse_the_plan() {
        let unavailable = marks_from(Err(anyhow::anyhow!("disk I/O error")));
        assert!(matches!(
            unavailable,
            Err(message)
                if message.starts_with("undo provenance unavailable")
                    && message.contains("disk I/O error")
        ));

        let marks = marks_from(Ok(vec!["abc".into()])).unwrap();
        assert!(marks.contains("abc"));
    }

    #[test]
    fn failed_mark_record_is_reported_in_the_result_note() {
        assert!(matches!(
            with_provenance_note(
                GitOpResult::Ok(None),
                "commit",
                Some("database unavailable".into())
            ),
            GitOpResult::Ok(Some(note))
                if note == "commit done; undo provenance not recorded: database unavailable"
        ));
        assert!(matches!(
            with_provenance_note(
                GitOpResult::Ok(Some("committed".into())),
                "commit",
                Some("database unavailable".into())
            ),
            GitOpResult::Ok(Some(note))
                if note == "committed; undo provenance not recorded: database unavailable"
        ));
        assert!(matches!(
            with_provenance_note(
                GitOpResult::Err("git failed".into()),
                "commit",
                Some("database unavailable".into())
            ),
            GitOpResult::Err(message) if message == "git failed"
        ));
    }

    #[test]
    fn cancelled_custom_outcome_has_quit_message() {
        let result = custom_command_result(
            (
                CUSTOM_OUTCOME_CANCELLED,
                None,
                String::new(),
                false,
                String::new(),
            ),
            true,
            false,
            CUSTOM_COMMAND_TIMEOUT_SECS,
        );
        assert!(matches!(
            result,
            GitOpResult::Err(message)
                if message == "custom command cancelled (thegn is quitting)"
        ));
    }

    // test code: fixture setup, never on the event loop.
    #[expect(clippy::disallowed_methods)]
    fn tmp_repo(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("tg-gitmut-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
        std::fs::create_dir_all(&dir).unwrap();
        let git = |args: &[&str]| {
            // `git -C dir` with GIT_DIR/GIT_WORK_TREE/etc. scrubbed so this
            // setup can't write to the OUTER repo's shared config when the
            // suite runs inside a commit hook (the core.worktree pollution bug).
            let ok = thegn_core::util::git_cmd(&dir)
                .args(args)
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

    fn commit_file_via_execute(dir: &std::path::Path, loc: &GitLoc, content: &str, message: &str) {
        std::fs::write(dir.join("f.txt"), content).unwrap();
        assert!(matches!(
            execute(GitOp::StageAll, loc, false),
            GitOpResult::Ok(_)
        ));
        assert!(matches!(
            execute(
                GitOp::Commit {
                    message: message.into(),
                    no_verify: false,
                    sign: Some(false),
                },
                loc,
                false,
            ),
            GitOpResult::Ok(_)
        ));
    }

    #[expect(clippy::disallowed_methods)]
    fn git_out(dir: &std::path::Path, args: &[&str]) -> String {
        let output = thegn_core::util::git_cmd(dir)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .unwrap();
        assert!(output.status.success(), "git {args:?}");
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
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
                no_verify: false,
                sign: Some(false),
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
        if let Err(error) = std::fs::remove_dir_all(&dir) {
            tracing::debug!(target: "thegn::gitmut::tests", %error, "failed to remove jj mutation test repo");
        }
    }

    #[test]
    fn execute_stage_untracked_lines_intent_adds_then_stages_selected() {
        let repo = tempfile::tempdir().unwrap();
        let dir = repo.path();
        git_out(dir, &["init", "-q", "-b", "main"]);
        git_out(dir, &["config", "user.name", "test"]);
        git_out(dir, &["config", "user.email", "t@example.invalid"]);
        std::fs::write(dir.join("base.txt"), "base\n").unwrap();
        git_out(dir, &["add", "base.txt"]);
        git_out(dir, &["commit", "-q", "-m", "base"]);

        std::fs::write(dir.join("new.txt"), "a\nb\nc\n").unwrap();
        let loc = GitLoc::Local(dir.to_path_buf());
        let synth_diff = CliGit.untracked_diff(&loc, "new.txt").unwrap();
        assert!(git_out(dir, &["status", "--porcelain"]).contains("?? new.txt"));
        let parsed = thegn_core::patch::parse_patch(&synth_diff);
        let indices = parsed
            .first()
            .into_iter()
            .flat_map(|file| {
                file.hunks.iter().enumerate().flat_map(|(hunk, part)| {
                    part.lines
                        .iter()
                        .enumerate()
                        .filter(|(_, line)| {
                            line.kind == thegn_core::patch::LineKind::Add && line.text == "b"
                        })
                        .map(move |(line, _)| (hunk, line))
                })
            })
            .collect::<Vec<_>>();
        assert_eq!(indices.len(), 1, "{synth_diff}");
        match execute(
            GitOp::StageUntrackedLines {
                path: "new.txt".into(),
                synth_diff,
                indices,
            },
            &loc,
            false,
        ) {
            GitOpResult::Ok(_) => {}
            other => panic!("{other:?}"),
        }
        let cached = git_out(dir, &["diff", "--cached", "--", "new.txt"]);
        assert!(cached.lines().any(|line| line == "+b"), "{cached}");
        assert!(!cached.lines().any(|line| line == "+a"), "{cached}");
        assert!(!cached.lines().any(|line| line == "+c"), "{cached}");

        std::fs::write(dir.join("changed.txt"), "before\n").unwrap();
        let stale = CliGit.untracked_diff(&loc, "changed.txt").unwrap();
        std::fs::write(dir.join("changed.txt"), "after\n").unwrap();
        assert!(matches!(
            execute(
                GitOp::StageUntrackedLines {
                    path: "changed.txt".into(),
                    synth_diff: stale,
                    indices: vec![(0, 0)],
                },
                &loc,
                false,
            ),
            GitOpResult::Err(message) if message.contains("file changed while staging; review and stage again")
        ));
    }

    #[test]
    fn execute_fixup_of_head_folds_and_finishes_the_rebase() {
        let dir = tmp_repo("fixup-head");
        let loc = GitLoc::Local(dir.clone());
        commit_file_via_execute(&dir, &loc, "two\n", "c1");
        let head = git_out(&dir, &["rev-parse", "HEAD"]);
        let tree_before = git_out(&dir, &["rev-parse", "HEAD^{tree}"]);

        assert!(matches!(
            execute(
                GitOp::Fixup {
                    oldest: head.clone(),
                    targets: vec![head],
                },
                &loc,
                false,
            ),
            GitOpResult::Ok(_)
        ));
        assert_eq!(git_out(&dir, &["rev-list", "--count", "HEAD"]), "1");
        assert_eq!(git_out(&dir, &["rev-parse", "HEAD^{tree}"]), tree_before);
        assert!(
            !dir.join(git_out(&dir, &["rev-parse", "--git-path", "rebase-merge"]))
                .exists()
        );
        if let Err(error) = std::fs::remove_dir_all(&dir) {
            tracing::debug!(target: "thegn::gitmut::tests", %error, "failed to remove fixup test repo");
        }
    }

    #[test]
    fn execute_squash_of_head_folds_and_finishes_the_rebase() {
        let dir = tmp_repo("squash-head");
        let loc = GitLoc::Local(dir.clone());
        commit_file_via_execute(&dir, &loc, "two\n", "c1");
        let head = git_out(&dir, &["rev-parse", "HEAD"]);
        let tree_before = git_out(&dir, &["rev-parse", "HEAD^{tree}"]);

        assert!(matches!(
            execute(
                GitOp::Squash {
                    oldest: head.clone(),
                    targets: vec![head],
                },
                &loc,
                false,
            ),
            GitOpResult::Ok(_)
        ));
        assert_eq!(git_out(&dir, &["rev-list", "--count", "HEAD"]), "1");
        assert_eq!(git_out(&dir, &["rev-parse", "HEAD^{tree}"]), tree_before);
        assert!(
            !dir.join(git_out(&dir, &["rev-parse", "--git-path", "rebase-merge"]))
                .exists()
        );
        if let Err(error) = std::fs::remove_dir_all(&dir) {
            tracing::debug!(target: "thegn::gitmut::tests", %error, "failed to remove squash test repo");
        }
    }

    #[test]
    fn execute_fixup_of_two_selected_commits_preserves_the_tree() {
        let dir = tmp_repo("fixup-range");
        let loc = GitLoc::Local(dir.clone());
        commit_file_via_execute(&dir, &loc, "two\n", "c1");
        commit_file_via_execute(&dir, &loc, "three\n", "c2");
        let commits = git_out(&dir, &["rev-list", "--reverse", "HEAD"]);
        let shas: Vec<String> = commits.lines().map(str::to_owned).collect();
        assert_eq!(shas.len(), 3);
        let tree_before = git_out(&dir, &["rev-parse", "HEAD^{tree}"]);

        assert!(matches!(
            execute(
                GitOp::Fixup {
                    oldest: shas[1].clone(),
                    targets: vec![shas[1].clone(), shas[2].clone()],
                },
                &loc,
                false,
            ),
            GitOpResult::Ok(_)
        ));
        assert_eq!(git_out(&dir, &["rev-list", "--count", "HEAD"]), "1");
        assert_eq!(git_out(&dir, &["rev-parse", "HEAD^{tree}"]), tree_before);
        assert!(
            !dir.join(git_out(&dir, &["rev-parse", "--git-path", "rebase-merge"]))
                .exists()
        );
        if let Err(error) = std::fs::remove_dir_all(&dir) {
            tracing::debug!(target: "thegn::gitmut::tests", %error, "failed to remove range test repo");
        }
    }

    #[test]
    fn index_mutations_report_jj_colocation_note() {
        let dir = tmp_repo("jj-note");
        std::fs::create_dir(dir.join(".jj")).unwrap();
        let loc = GitLoc::Local(dir.clone());
        std::fs::write(dir.join("f.txt"), "two\n").unwrap();
        match execute(GitOp::StageAll, &loc, false) {
            GitOpResult::Ok(Some(note)) => assert!(note.contains("jj ignores the git index")),
            other => panic!("{other:?}"),
        }
        match execute(
            GitOp::Commit {
                message: "m".into(),
                no_verify: false,
                sign: Some(false),
            },
            &loc,
            false,
        ) {
            GitOpResult::Ok(Some(note)) => assert!(note.contains("jj ignores the git index")),
            other => panic!("{other:?}"),
        }
        if let Err(error) = std::fs::remove_dir_all(&dir) {
            tracing::debug!(target: "thegn::gitmut::tests", %error, "failed to remove jj mutation test repo");
        }
    }

    #[test]
    fn index_mutations_only_note_a_jj_directory() {
        let plain = tmp_repo("jj-note-plain");
        std::fs::write(plain.join("f.txt"), "two\n").unwrap();
        match execute(GitOp::StageAll, &GitLoc::Local(plain.clone()), false) {
            GitOpResult::Ok(None) => {}
            other => panic!("{other:?}"),
        }

        let jj_file = tmp_repo("jj-note-file");
        std::fs::write(jj_file.join(".jj"), "not a directory").unwrap();
        std::fs::write(jj_file.join("f.txt"), "two\n").unwrap();
        match execute(GitOp::StageAll, &GitLoc::Local(jj_file.clone()), false) {
            GitOpResult::Ok(None) => {}
            other => panic!("{other:?}"),
        }
        if let Err(error) = std::fs::remove_dir_all(&plain) {
            tracing::debug!(target: "thegn::gitmut::tests", %error, "failed to remove plain git mutation test repo");
        }
        if let Err(error) = std::fs::remove_dir_all(&jj_file) {
            tracing::debug!(target: "thegn::gitmut::tests", %error, "failed to remove jj-file mutation test repo");
        }
    }

    #[test]
    fn discard_files_restores_tracked_and_removes_untracked() {
        // Item 604: batch rollback partitions tracked vs untracked.
        let dir = tmp_repo("rollback");
        let loc = GitLoc::Local(dir.clone());
        // Tracked, modified.
        std::fs::write(dir.join("f.txt"), "dirty\n").unwrap();
        // Untracked, new.
        std::fs::write(dir.join("new.txt"), "scratch\n").unwrap();
        match execute(
            GitOp::DiscardFiles {
                paths: vec![("f.txt".into(), false), ("new.txt".into(), true)],
            },
            &loc,
            false,
        ) {
            GitOpResult::Ok(Some(note)) => assert!(note.contains("2 file(s)")),
            other => panic!("{other:?}"),
        }
        // Tracked file restored to HEAD; untracked file deleted.
        assert_eq!(std::fs::read_to_string(dir.join("f.txt")).unwrap(), "one\n");
        assert!(!dir.join("new.txt").exists());
        let _ = std::fs::remove_dir_all(&dir); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
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
                message: String::new(),
                no_verify: false,
                sign: None,
            }
            .rewrites_history()
        );
    }

    #[test]
    fn line_stage_guard_rejects_atomic_submodule_pointer() {
        let diff = concat!(
            "diff --git a/vendor/lib b/vendor/lib\n",
            "index aaaaaaa..bbbbbbb 160000\n",
            "--- a/vendor/lib\n",
            "+++ b/vendor/lib\n",
            "@@ -1 +1 @@\n",
            "-Subproject commit aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n",
            "+Subproject commit bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\n",
        );
        assert_eq!(
            validate_line_selection(diff, &[(0, 0)]),
            Err("submodule pointers are atomic".into())
        );
    }

    #[test]
    fn failing_pre_commit_hook_routes_to_hook_failed_popup() {
        // Item 329: a rejecting hook's multi-line output lands in HookFailed
        // (popup), and `no_verify` bypasses to a clean commit.
        let dir = tmp_repo("hook");
        let loc = GitLoc::Local(dir.clone());
        let hook = dir.join(".git/hooks/pre-commit");
        std::fs::write(&hook, "#!/bin/sh\necho 'LINT FAILED'\nexit 1\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::fs::write(dir.join("f.txt"), "two\n").unwrap();
        let _ = execute(GitOp::StageAll, &loc, false); // best-effort: test setup: the commit assert below catches a failed stage
        match execute(
            GitOp::Commit {
                message: "blocked".into(),
                no_verify: false,
                sign: None,
            },
            &loc,
            false,
        ) {
            GitOpResult::HookFailed { output } => assert!(output.contains("LINT FAILED")),
            other => panic!("expected HookFailed, got {other:?}"),
        }
        match execute(
            GitOp::Commit {
                message: "forced".into(),
                no_verify: true,
                sign: None,
            },
            &loc,
            false,
        ) {
            GitOpResult::Ok(_) => {}
            other => panic!("expected Ok, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
    }

    #[cfg(unix)]
    #[test]
    fn hook_failed_popup_sanitizes_terminal_controls_and_redacts_secrets() {
        let dir = tmp_repo("hook-popup-redaction");
        let loc = GitLoc::Local(dir.clone());
        let hook = dir.join(".git/hooks/pre-commit");
        std::fs::write(
            &hook,
            "#!/bin/sh\nprintf 'LINT FAILED\\r\\033]0;x\\007token https://u:hunter2@h.invalid\\n'; exit 1\n",
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::fs::write(dir.join("f.txt"), "two\n").unwrap();
        assert!(matches!(
            execute(GitOp::StageAll, &loc, false),
            GitOpResult::Ok(_)
        ));
        match execute(
            GitOp::Commit {
                message: "blocked by hostile hook".into(),
                no_verify: false,
                sign: None,
            },
            &loc,
            false,
        ) {
            GitOpResult::HookFailed { output } => {
                assert!(output.contains("LINT FAILED"));
                assert!(!output.contains('\r'));
                assert!(!output.contains('\u{1b}'));
                assert!(!output.contains("hunter2"));
                assert!(output.contains(thegn_core::ci_log::REDACTION_MARKER));
            }
            other => panic!("expected sanitized HookFailed, got {other:?}"),
        }
        if let Err(error) = std::fs::remove_dir_all(&dir) {
            tracing::debug!(%error, "hostile hook test repo cleanup failed");
        }
    }

    #[cfg(unix)]
    #[test]
    fn custom_popup_marks_truncated_output() {
        let dir = tmp_repo("custom-popup-cap");
        let loc = GitLoc::Local(dir.clone());
        match execute(
            GitOp::Custom {
                command: "head -c 5000000 /dev/zero".into(),
                capture: true,
            },
            &loc,
            false,
        ) {
            GitOpResult::Output(output) => {
                assert!(output.len() <= 1024 * 1024 + 40);
                assert!(output.ends_with("… output truncated at 1 MiB"));
            }
            other => panic!("expected truncated popup output, got {other:?}"),
        }
        if let Err(error) = std::fs::remove_dir_all(&dir) {
            tracing::debug!(%error, "custom popup test repo cleanup failed");
        }
    }

    #[cfg(unix)]
    #[test]
    fn custom_template_uses_configured_timeout_in_error() {
        let dir = tmp_repo("custom-template-timeout");
        let loc = GitLoc::Local(dir.clone());
        let configured: thegn_core::config::GitCommand = toml::from_str(
            r#"
key = "x"
argv = ["sh", "-c", "sleep 30"]
output = "popup"
timeout_secs = 1
"#,
        )
        .unwrap();
        let command = thegn_core::custom_cmd::compile(
            &configured,
            &thegn_core::custom_cmd::TemplateCtx::default(),
        )
        .unwrap();
        let started = std::time::Instant::now();
        let result = execute(
            GitOp::CustomTemplate {
                command,
                capture: true,
            },
            &loc,
            false,
        );
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        assert!(matches!(result, GitOpResult::Err(message) if message.contains("after 1s")));
        if let Err(error) = std::fs::remove_dir_all(&dir) {
            tracing::debug!(%error, "custom template timeout test repo cleanup failed");
        }
    }

    #[cfg(unix)]
    #[test]
    fn custom_popup_output_redacts_secrets_and_strips_terminal_controls() {
        let dir = tmp_repo("custom-popup-redaction");
        let loc = GitLoc::Local(dir.clone());
        match execute(
            GitOp::Custom {
                command: "printf 'https://user:hunter2@example.invalid/r.git\\nghp_1234567890\\033]0;a b\\007ABCDEFGHIJ1234567890\\nline\\rpaint\\n'".into(),
                capture: true,
            },
            &loc,
            false,
        ) {
            GitOpResult::Output(output) => {
                assert!(!output.contains("hunter2"));
                assert!(!output.contains("ghp_1234567890"));
                assert!(!output.contains('\u{1b}'));
                assert!(!output.contains('\u{7}'));
                assert!(!output.contains('\r'));
                assert!(output.contains(thegn_core::ci_log::REDACTION_MARKER));
            }
            other => panic!("expected sanitized custom popup output, got {other:?}"),
        }
        if let Err(error) = std::fs::remove_dir_all(&dir) {
            tracing::debug!(%error, "custom popup redaction test repo cleanup failed");
        }
    }

    #[cfg(unix)]
    #[test]
    fn custom_none_mode_discards_output_body() {
        // Behaviour guard for the host mapping; the svc-level test also
        // asserts that the bounded runner retains no stdout body.
        let dir = tmp_repo("custom-none");
        let loc = GitLoc::Local(dir.clone());
        assert!(matches!(
            execute(
                GitOp::Custom {
                    command: "printf 'output that must be discarded'".into(),
                    capture: false,
                },
                &loc,
                false,
            ),
            GitOpResult::Ok(None)
        ));
        if let Err(error) = std::fs::remove_dir_all(&dir) {
            tracing::debug!(%error, "custom none test repo cleanup failed");
        }
    }

    #[cfg(unix)]
    #[test]
    fn custom_failure_redacts_secrets_and_sanitizes_terminal_controls() {
        let dir = tmp_repo("custom-failure-redaction");
        let loc = GitLoc::Local(dir.clone());
        match execute(
            GitOp::Custom {
                command: "printf 'fatal: https://user:hunter2@example.invalid/r.git\\nghp_1234567890\\033]0;a b\\007ABCDEFGHIJ1234567890\\n\\033]0;owned\\007bad\\033[31mred\\n' >&2; exit 3".into(),
                capture: false,
            },
            &loc,
            false,
        ) {
            GitOpResult::Err(msg) => {
                assert!(msg.starts_with("command failed"));
                assert!(!msg.contains("hunter2"));
                assert!(!msg.contains("ghp_1234567890"));
                assert!(msg.contains(thegn_core::ci_log::REDACTION_MARKER));
                assert!(!msg.contains('\u{1b}'));
                assert!(!msg.contains('\u{7}'));
            }
            other => panic!("expected custom command failure, got {other:?}"),
        }
        if let Err(error) = std::fs::remove_dir_all(&dir) {
            tracing::debug!(%error, "custom failure redaction test repo cleanup failed");
        }
    }

    #[test]
    fn execute_stash_drop_by_sha_drops_the_selected_stash() {
        let repo = tempfile::tempdir().unwrap();
        let dir = repo.path();
        git_out(dir, &["init", "-q", "-b", "main"]);
        git_out(dir, &["config", "user.name", "t"]);
        git_out(dir, &["config", "user.email", "t@e"]);
        git_out(dir, &["config", "commit.gpgsign", "false"]);
        std::fs::write(dir.join("f.txt"), "base\n").unwrap();
        git_out(dir, &["add", "f.txt"]);
        git_out(dir, &["commit", "-q", "-m", "initial"]);
        let loc = GitLoc::Local(dir.to_path_buf());
        std::fs::write(dir.join("f.txt"), "stash A\n").unwrap();
        git_out(dir, &["stash", "push", "-m", "stash A"]);
        let sha_a = git_out(dir, &["rev-parse", "stash@{0}"]);
        std::fs::write(dir.join("f.txt"), "stash B\n").unwrap();
        git_out(dir, &["stash", "push", "-m", "stash B"]);
        let sha_b = git_out(dir, &["rev-parse", "stash@{0}"]);

        match execute(GitOp::StashDrop { sha: sha_a }, &loc, false) {
            GitOpResult::Ok(_) => {}
            other => panic!("expected stash drop success, got {other:?}"),
        }

        assert_eq!(git_out(dir, &["rev-parse", "stash@{0}"]), sha_b);
        assert_eq!(
            git_out(dir, &["stash", "list", "--format=%H"])
                .lines()
                .count(),
            1
        );
    }

    #[test]
    fn execute_for_worktree_refuses_when_the_location_is_unreadable() {
        let dir = tmp_repo("loc-unreadable");
        std::fs::write(dir.join("new.txt"), "new\n").unwrap();
        let state = crate::testenv::IsolatedState::new();
        std::fs::create_dir_all(state.state_home().join("thegn/thegn.db")).unwrap();
        let result = execute_for_worktree(
            GitOp::StageFile {
                path: "new.txt".into(),
                unstage: false,
            },
            &dir,
            false,
        );
        assert!(matches!(result, GitOpResult::Err(message) if message.contains("refused")));
        assert!(git_out(&dir, &["diff", "--cached", "--name-only"]).is_empty());
    }

    #[test]
    fn control_stage_refuses_when_the_location_is_unreadable() {
        let dir = tmp_repo("stage-loc-unreadable");
        std::fs::write(dir.join("new.txt"), "new\n").unwrap();
        let state = crate::testenv::IsolatedState::new();
        std::fs::create_dir_all(state.state_home().join("thegn/thegn.db")).unwrap();
        let result = control_stage(&dir, &["new.txt".into()]);
        assert!(matches!(result, Err(error) if format!("{error:#}").contains("git stage refused")));
        assert!(git_out(&dir, &["diff", "--cached", "--name-only"]).is_empty());
    }

    #[test]
    fn control_commit_refuses_when_the_location_is_unreadable() {
        let dir = tmp_repo("commit-loc-unreadable");
        std::fs::write(dir.join("f.txt"), "changed\n").unwrap();
        git_out(&dir, &["add", "f.txt"]);
        let head = git_out(&dir, &["rev-parse", "HEAD"]);
        let state = crate::testenv::IsolatedState::new();
        std::fs::create_dir_all(state.state_home().join("thegn/thegn.db")).unwrap();
        let result = control_commit(&dir, "msg");
        assert!(
            matches!(result, Err(error) if format!("{error:#}").contains("git commit refused"))
        );
        assert_eq!(git_out(&dir, &["rev-parse", "HEAD"]), head);
    }

    #[test]
    fn control_commit_reads_head_through_the_recorded_location() {
        use thegn_core::store::WorkspaceStore;
        let _state = crate::testenv::IsolatedState::new();
        let host = tmp_repo("commit-head-host");
        let remote = tmp_repo("commit-head-remote");
        std::fs::write(host.join("host-only.txt"), "host history\n").unwrap();
        git_out(&host, &["add", "host-only.txt"]);
        git_out(&host, &["commit", "-m", "host-only"]);
        let db = thegn_core::db::Db::open().unwrap();
        db.put_worktree(
            "repo/x",
            &host.to_string_lossy(),
            &host.to_string_lossy(),
            "main",
            Some(&GitLoc::provider_db_string(
                &["env".into()],
                &remote.to_string_lossy(),
            )),
            None,
        )
        .unwrap();
        drop(db);

        std::fs::write(remote.join("new.txt"), "remote change\n").unwrap();
        git_out(&remote, &["add", "new.txt"]);
        let head = control_commit(&host, "msg").unwrap();
        assert_eq!(head, git_out(&remote, &["rev-parse", "HEAD"]));
        assert_ne!(head, git_out(&host, &["rev-parse", "HEAD"]));
    }

    #[test]
    fn execute_for_worktree_runs_on_a_readable_local_worktree() {
        let _state = crate::testenv::IsolatedState::new();
        thegn_core::db::Db::open().unwrap();
        let dir = tmp_repo("loc-readable");
        std::fs::write(dir.join("new.txt"), "new\n").unwrap();
        let result = execute_for_worktree(
            GitOp::StageFile {
                path: "new.txt".into(),
                unstage: false,
            },
            &dir,
            false,
        );
        assert!(matches!(result, GitOpResult::Ok(_)));
        assert_eq!(
            git_out(&dir, &["diff", "--cached", "--name-only"]),
            "new.txt"
        );
    }
}
