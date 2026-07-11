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
//! `rebase-merge/git-rebase-todo` can be re-read and rewritten — safe
//! because the host serializes mutations per worktree.

use super::{GitBackend, gpg_args, run_w};
use anyhow::{Context, Result, anyhow, bail};
use thegn_core::rebase_todo::{
    TodoAction, TodoEntry, parse_todo, place_fixup, retag, serialize_todo, todo_from_log,
};
use thegn_core::remote::GitLoc;
use thegn_core::util::sh_quote;

/// Scratch file (inside the private gitdir) holding the prepared todo.
const TODO_SCRATCH: &str = "thegn-todo";

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
    loc.write_git_path(TODO_SCRATCH, serialize_todo(todo).as_bytes())
        .context("write prepared rebase todo")?;
    let scratch = loc
        .git_out(&["rev-parse", "--git-path", TODO_SCRATCH])
        .ok_or_else(|| anyhow!("cannot resolve todo scratch path"))?;
    let scratch = if scratch.starts_with('/') {
        scratch
    } else {
        format!("{}/{scratch}", loc.path())
    };
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
        // Squash-message and reword stops open $GIT_EDITOR; `:` accepts the
        // default message so the rebase never blocks on a terminal.
        ("GIT_EDITOR", ":"),
    ];
    classify(loc, run_w(loc, &envs, &args))
}

/// Map a rebase invocation result onto Done/Conflict/Paused by probing the
/// repo. Both exit statuses need the probe: `edit`/`break` stops exit ZERO
/// (a zero exit with `rebase-merge/` still present is a pause, not
/// completion), while conflicts exit non-zero; a non-zero exit with no
/// rebase in progress is a real error.
fn classify(loc: &GitLoc, res: Result<String>) -> Result<RebaseOutcome> {
    let rebasing = loc.read_git_path("rebase-merge/onto").is_some()
        || loc.read_git_path("rebase-apply/onto").is_some();
    match res {
        Ok(_) if !rebasing => Ok(RebaseOutcome::Done),
        Ok(_) => Ok(RebaseOutcome::Paused),
        Err(e) => {
            if !rebasing {
                return Err(e);
            }
            let conflicted = loc
                .git_out(&["diff", "--name-only", "--diff-filter=U"])
                .is_some();
            if conflicted {
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
    match loc.git_out(&["rev-parse", "--verify", "-q", &format!("{sha}^")]) {
        Some(parent) => Ok(parent),
        None => Ok("--root".to_string()),
    }
}

/// Generate the pick-everything todo for `base..HEAD` (oldest first).
fn todo_for(loc: &GitLoc, base: &str) -> Result<Vec<TodoEntry>> {
    let range = if base == "--root" {
        "HEAD".to_string()
    } else {
        format!("{base}..HEAD")
    };
    let out = run_w(
        loc,
        &[],
        &["log", "--reverse", "--format=%x1f%H%x1f%s", &range],
    )?;
    let todo = todo_from_log(&out);
    if todo.is_empty() {
        bail!("no commits to rebase");
    }
    // Merge commits cannot be linearized by a plain rebase -i; refuse with a
    // clear error instead of silently dropping them (no --rebase-merges yet).
    let merges = run_w(loc, &[], &["log", "--merges", "--format=%h", &range])?;
    if !merges.trim().is_empty() {
        bail!("range contains merge commits — rebase them manually");
    }
    Ok(todo)
}

pub trait RebaseOps: GitBackend {
    /// The pick-everything todo for `<sha>^..HEAD` — the seed the
    /// interactive-rebase view edits before `rebase_interactive`.
    fn rebase_todo_for(&self, loc: &GitLoc, oldest_sha: &str) -> Result<Vec<TodoEntry>> {
        todo_for(loc, &base_for(loc, oldest_sha)?)
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

    /// Once-off: retag `targets` (squash/fixup/drop/edit) and rebase. The
    /// todo spans from the oldest target's parent to HEAD.
    fn rebase_retag(
        &self,
        loc: &GitLoc,
        oldest_sha: &str,
        targets: &[&str],
        action: TodoAction,
        opts: &RebaseOpts,
    ) -> Result<RebaseOutcome> {
        let base = base_for(loc, oldest_sha)?;
        let todo = todo_for(loc, &base)?;
        let todo = retag(&todo, targets, action)?;
        rebase_with_todo(loc, &base, &todo, opts)
    }

    /// Once-off: move `sha` one position up (older) or down (newer).
    fn rebase_move(
        &self,
        loc: &GitLoc,
        sha: &str,
        up: bool,
        opts: &RebaseOpts,
    ) -> Result<RebaseOutcome> {
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
    }

    /// Reword a commit. HEAD rewords amend in place; older commits stop at
    /// the target with `edit`, amend `--only` (so currently-staged changes
    /// are NOT swept into the commit), and continue. git refuses to start a
    /// rebase while the index has uncommitted changes, so staged changes are
    /// parked in a staged-only stash for the duration and restored — still
    /// staged — via `stash pop --index`.
    fn reword(&self, loc: &GitLoc, sha: &str, message: &str, opts: &RebaseOpts) -> Result<()> {
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
            match self.rebase_continue(loc)? {
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
        let Some(onto) = loc.read_git_path("rebase-merge/onto") else {
            return Ok(None);
        };
        let text = |b: Option<Vec<u8>>| {
            b.map(|b| String::from_utf8_lossy(&b).trim().to_string())
                .unwrap_or_default()
        };
        let conflicted = loc
            .git_out(&["diff", "--name-only", "--diff-filter=U"])
            .is_some();
        Ok(Some(RebaseStatus {
            onto: text(Some(onto)),
            head_name: text(loc.read_git_path("rebase-merge/head-name"))
                .trim_start_matches("refs/heads/")
                .to_string(),
            stopped_sha: {
                let s = text(loc.read_git_path("rebase-merge/stopped-sha"));
                (!s.is_empty()).then_some(s)
            },
            done: parse_todo(&text(loc.read_git_path("rebase-merge/done"))),
            remaining: parse_todo(&text(loc.read_git_path("rebase-merge/git-rebase-todo"))),
            paused: if conflicted {
                PauseReason::Conflict
            } else {
                PauseReason::Edit
            },
        }))
    }

    /// Rewrite the PENDING entries of a paused rebase (reorder/drop/retag
    /// mid-flight). Only safe because the host serializes mutations — git
    /// must not be running against this worktree.
    fn rewrite_pending_todo(&self, loc: &GitLoc, todo: &[TodoEntry]) -> Result<()> {
        if loc.read_git_path("rebase-merge/git-rebase-todo").is_none() {
            bail!("no interactive rebase in progress");
        }
        loc.write_git_path(
            "rebase-merge/git-rebase-todo",
            serialize_todo(todo).as_bytes(),
        )
        .context("rewrite live rebase todo")
    }

    /// [`rewrite_pending_todo`](Self::rewrite_pending_todo) with a
    /// lost-update guard: `baseline` is the pending list AS THE EDITOR READ
    /// IT, and the write is refused when the on-disk todo no longer parses
    /// to the same entries (the user edited it from another terminal, or
    /// the sequencer moved). Comparison is over parsed entries, so git's
    /// regenerable `#` comment noise can never cause a false conflict.
    fn rewrite_pending_todo_checked(
        &self,
        loc: &GitLoc,
        todo: &[TodoEntry],
        baseline: &[TodoEntry],
    ) -> Result<()> {
        let Some(disk) = loc.read_git_path("rebase-merge/git-rebase-todo") else {
            bail!("no interactive rebase in progress");
        };
        let disk = parse_todo(&String::from_utf8_lossy(&disk));
        if disk != baseline {
            bail!("rebase todo changed on disk — reload the editor before rewriting");
        }
        loc.write_git_path(
            "rebase-merge/git-rebase-todo",
            serialize_todo(todo).as_bytes(),
        )
        .context("rewrite live rebase todo")
    }
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

    /// File names in HEAD's tree.
    fn tree(r: &TestRepo) -> String {
        r.out(&["ls-tree", "--name-only", "HEAD"])
    }

    /// File names touched by `sha`.
    fn touched(r: &TestRepo, sha: &str) -> String {
        r.out(&["show", "--name-only", "--format=", sha])
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
}
