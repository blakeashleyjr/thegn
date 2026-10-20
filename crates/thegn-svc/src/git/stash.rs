//! Stash operations, including the staged-only (git ≥ 2.35) and
//! unstaged-only (temp-commit dance) variants.

use super::{GitBackend, run_w, run_w_locked};
use anyhow::{Result, bail};
use thegn_core::gitrefs::{parse_git_version, supports_stash_staged};
use thegn_core::remote::GitLoc;

#[cfg(test)]
type UnstagedStashStepHook = Box<dyn FnMut(&str)>;

#[cfg(test)]
thread_local! {
    static UNSTAGED_STASH_STEP_HOOK: std::cell::RefCell<Option<UnstagedStashStepHook>> =
        std::cell::RefCell::new(None);
}

fn at(index: usize) -> String {
    format!("stash@{{{index}}}")
}

fn stash_index_for(loc: &GitLoc, sha: &str) -> Result<Option<usize>> {
    let (exit, out) = super::run_status(
        loc,
        &["stash", "list", "-z", "--format=%gd%x00%H%x00%ct%x00%gs"],
    )?;
    if exit != 0 {
        bail!("git stash list exited with status {exit}");
    }
    Ok(thegn_core::gitrefs::parse_stashes(&out)?
        .into_iter()
        .find(|entry| entry.sha == sha)
        .map(|entry| entry.index))
}

fn require_index(sha: &str, index: Option<usize>) -> Result<usize> {
    match index {
        Some(index) => Ok(index),
        None => {
            let short: String = sha.chars().take(7).collect();
            bail!("stash {short} no longer exists (the stash list changed); refresh and retry");
        }
    }
}

fn stash_mutate(loc: &GitLoc, verb: &str, sha: &str) -> Result<()> {
    if loc.is_remote() || crate::bridge::for_loc(loc).is_some() {
        let index = require_index(sha, stash_index_for(loc, sha)?)?;
        run_w(loc, &[], &["stash", verb, &at(index)]).map(|_| ())
    } else {
        let lock = super::require_mutation_lock_within(
            std::path::Path::new(&loc.path()),
            &["stash", verb],
            super::MUTATION_LOCK_WAIT,
        )?;
        let index = require_index(sha, stash_index_for(loc, sha)?)?;
        let selector = at(index);
        run_w_locked(loc, &[], &["stash", verb, &selector], &lock).map(|_| ())
    }
}

pub trait StashOps: GitBackend {
    /// `git stash push [-u] -m <msg>`.
    fn stash_push(&self, loc: &GitLoc, message: &str, include_untracked: bool) -> Result<()> {
        let mut args = vec!["stash", "push"];
        if include_untracked {
            args.push("-u");
        }
        args.extend(["-m", message]);
        run_w(loc, &[], &args).map(|_| ())
    }

    /// Stash only the staged changes (`--staged`, git ≥ 2.35 — probed via
    /// `git version` so older gits get a clear error, not a flag complaint).
    fn stash_staged(&self, loc: &GitLoc, message: &str) -> Result<()> {
        let v = loc
            .git_out(&["version"])
            .and_then(|s| parse_git_version(&s));
        if !v.is_some_and(supports_stash_staged) {
            bail!("`git stash push --staged` needs git >= 2.35");
        }
        run_w(loc, &[], &["stash", "push", "--staged", "-m", message]).map(|_| ())
    }

    /// Stash only the UNSTAGED changes: park the index in a temp commit,
    /// stash what remains (incl. untracked), then soft-reset the temp commit
    /// away — the index comes back exactly as it was.
    fn stash_unstaged(&self, loc: &GitLoc, message: &str) -> Result<()> {
        stash_unstaged_within(loc, message, super::MUTATION_LOCK_WAIT)
    }

    /// Resolve the stash index at mutation time under the local git lock.
    fn stash_pop(&self, loc: &GitLoc, sha: &str) -> Result<()> {
        stash_mutate(loc, "pop", sha)
    }

    /// Resolve the stash index at mutation time under the local git lock.
    fn stash_apply(&self, loc: &GitLoc, sha: &str) -> Result<()> {
        stash_mutate(loc, "apply", sha)
    }

    /// Resolve the stash index at mutation time under the local git lock.
    fn stash_drop(&self, loc: &GitLoc, sha: &str) -> Result<()> {
        stash_mutate(loc, "drop", sha)
    }

    /// Read a stash's patch without taking the mutation lock or running
    /// repository-configured hooks, external diff drivers, or textconv. Porcelain
    /// `stash show` is textconv-off by default, so these flags are defense in depth
    /// against user-level `diff.*`/alias config; the read runner is the real fence.
    /// `-u` includes the untracked third-parent tree that plain
    /// `stash show -p` silently omits.
    fn stash_show(&self, loc: &GitLoc, rev: &str) -> Result<String> {
        let rev = super::op::revision(rev)?;
        super::run(
            loc,
            &[
                "stash",
                "show",
                "-p",
                "-u",
                "--no-color",
                "--no-ext-diff",
                "--no-textconv",
                rev,
            ],
        )
    }
}

impl<T: GitBackend + ?Sized> StashOps for T {}

fn stash_unstaged_within(loc: &GitLoc, message: &str, wait: std::time::Duration) -> Result<()> {
    let _txn = if !loc.is_remote() && crate::bridge::for_loc(loc).is_none() {
        super::begin_mutation_txn(loc, &["commit", "--no-verify", "--allow-empty"], wait)?
    } else {
        None
    };
    run_w(
        loc,
        &[],
        &[
            "commit",
            "--no-verify",
            "--allow-empty",
            "-m",
            "[thegn] temp index",
        ],
    )?;
    let stashed = run_w(loc, &[], &["stash", "push", "-u", "-m", message]);
    // Always restore the index, even when the stash failed (e.g. nothing
    // unstaged to stash).
    #[cfg(test)]
    UNSTAGED_STASH_STEP_HOOK.with(|hook| {
        if let Some(hook) = hook.borrow_mut().as_mut() {
            hook("restore");
        }
    });
    let restored = run_w(loc, &[], &["reset", "--soft", "HEAD^"]);
    match (stashed, restored) {
        (Ok(_), Ok(_)) => Ok(()),
        (Err(stash), Ok(_)) => Err(stash),
        (Ok(_), Err(restore)) => Err(restore.context(
            "stash saved, but restoring the index failed; the temp commit \"[thegn] temp index\" is still on HEAD",
        )),
        (Err(stash), Err(restore)) => Err(stash.context(format!(
            "restoring the index after a failed stash also failed ({restore}); the temp commit \"[thegn] temp index\" is still on HEAD"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::super::testutil::{TestRepo, git_in};
    use super::super::{CliGit, GitBackend};
    use super::{StashOps, UNSTAGED_STASH_STEP_HOOK, require_index, stash_unstaged_within};
    use std::path::Path;
    use thegn_core::gitrefs::{parse_git_version, supports_stash_staged};

    /// Ops run through `GitLoc` (the user's real git env, not the testutil
    /// env), so the repo itself needs an identity and gpg pinned off.
    fn ident(dir: &Path) {
        git_in(dir, &["config", "user.name", "t"]);
        git_in(dir, &["config", "user.email", "t@e"]);
        git_in(dir, &["config", "commit.gpgsign", "false"]);
    }

    fn read(repo: &TestRepo, path: &str) -> String {
        std::fs::read_to_string(repo.dir.join(path)).unwrap()
    }

    #[test]
    fn require_index_formats_the_vanished_stash_error_once() {
        let error = require_index("1234567890abcdef", None).unwrap_err();

        assert_eq!(
            error.to_string(),
            "stash 1234567 no longer exists (the stash list changed); refresh and retry"
        );
    }

    #[test]
    fn stash_push_list_pop_roundtrip_with_untracked() {
        let repo = TestRepo::new("st-roundtrip");
        ident(&repo.dir);
        repo.commit_file("f.txt", "one\n", "c0");
        std::fs::write(repo.dir.join("f.txt"), "two\n").unwrap();
        std::fs::write(repo.dir.join("u.txt"), "new\n").unwrap();
        let loc = repo.loc();

        CliGit.stash_push(&loc, "wip stuff", true).unwrap();
        assert!(
            CliGit.status(&loc).unwrap().is_empty(),
            "-u swept everything"
        );
        assert_eq!(read(&repo, "f.txt"), "one\n");
        assert!(!repo.dir.join("u.txt").exists());
        let list = CliGit.stash_list(&loc).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].index, 0);
        assert!(
            list[0].message.contains("wip stuff"),
            "{:?}",
            list[0].message
        );

        let sha = CliGit.stash_list(&loc).unwrap()[0].sha.clone();
        CliGit.stash_pop(&loc, &sha).unwrap();
        assert!(
            CliGit.stash_list(&loc).unwrap().is_empty(),
            "pop drops the entry"
        );
        assert_eq!(read(&repo, "f.txt"), "two\n");
        assert_eq!(read(&repo, "u.txt"), "new\n");
    }

    #[test]
    fn stash_apply_keeps_entry_and_drop_removes_it() {
        let repo = TestRepo::new("st-applydrop");
        ident(&repo.dir);
        repo.commit_file("f.txt", "one\n", "c0");
        std::fs::write(repo.dir.join("f.txt"), "two\n").unwrap();
        let loc = repo.loc();
        CliGit.stash_push(&loc, "keepme", false).unwrap();

        let sha = CliGit.stash_list(&loc).unwrap()[0].sha.clone();
        CliGit.stash_apply(&loc, &sha).unwrap();
        assert_eq!(read(&repo, "f.txt"), "two\n");
        assert_eq!(CliGit.stash_list(&loc).unwrap().len(), 1, "apply keeps it");

        CliGit.stash_drop(&loc, &sha).unwrap();
        assert!(CliGit.stash_list(&loc).unwrap().is_empty());
        assert_eq!(
            read(&repo, "f.txt"),
            "two\n",
            "drop leaves the worktree alone"
        );
    }

    #[test]
    fn stash_staged_takes_only_the_index() {
        let repo = TestRepo::new("st-staged");
        ident(&repo.dir);
        repo.commit_file("a.txt", "a1\n", "c0");
        repo.commit_file("b.txt", "b1\n", "c1");
        let loc = repo.loc();

        let v = parse_git_version(&repo.out(&["version"]));
        if !v.is_some_and(supports_stash_staged) {
            // Old git: the op must refuse with a clear error, not run.
            assert!(CliGit.stash_staged(&loc, "staged only").is_err());
            return;
        }

        std::fs::write(repo.dir.join("a.txt"), "a2\n").unwrap();
        git_in(&repo.dir, &["add", "a.txt"]);
        std::fs::write(repo.dir.join("b.txt"), "b2\n").unwrap();

        CliGit.stash_staged(&loc, "staged only").unwrap();
        assert_eq!(read(&repo, "b.txt"), "b2\n", "unstaged change stays put");
        assert_eq!(
            read(&repo, "a.txt"),
            "a1\n",
            "staged change left with the stash"
        );
        assert!(repo.out(&["diff", "--cached", "--name-only"]).is_empty());
        let patch = repo.out(&["stash", "show", "-p", "stash@{0}"]);
        assert!(patch.contains("a.txt"), "{patch}");
        assert!(!patch.contains("b.txt"), "{patch}");
    }

    #[test]
    fn stash_unstaged_takes_worktree_changes_and_preserves_the_index() {
        let repo = TestRepo::new("st-unstaged");
        ident(&repo.dir);
        repo.commit_file("a.txt", "a1\n", "c0");
        repo.commit_file("b.txt", "b1\n", "c1");
        let head = repo.head();
        let loc = repo.loc();

        std::fs::write(repo.dir.join("a.txt"), "a2\n").unwrap();
        git_in(&repo.dir, &["add", "a.txt"]);
        std::fs::write(repo.dir.join("b.txt"), "b2\n").unwrap();
        let staged_before = repo.out(&["diff", "--cached"]);

        CliGit.stash_unstaged(&loc, "unstaged only").unwrap();

        assert_eq!(repo.head(), head, "temp commit was soft-reset away");
        assert_eq!(
            repo.out(&["diff", "--cached"]),
            staged_before,
            "index restored exactly"
        );
        assert_eq!(read(&repo, "b.txt"), "b1\n", "unstaged change stashed");
        assert_eq!(
            read(&repo, "a.txt"),
            "a2\n",
            "staged content stays in the worktree"
        );
        let list = CliGit.stash_list(&loc).unwrap();
        assert_eq!(list.len(), 1);
        let patch = repo.out(&["stash", "show", "-p", "stash@{0}"]);
        assert!(patch.contains("b.txt"), "{patch}");
        assert!(!patch.contains("a.txt"), "{patch}");
    }

    #[test]
    fn stash_unstaged_refuses_whole_sequence_under_a_held_txn() {
        let repo = TestRepo::new("st-unstaged-txn");
        ident(&repo.dir);
        repo.commit_file("f.txt", "base\n", "c0");
        std::fs::write(repo.dir.join("f.txt"), "dirty\n").unwrap();
        std::fs::write(repo.dir.join("untracked.txt"), "new\n").unwrap();
        let loc = repo.loc();
        let head = repo.head();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let worktree = repo.dir.clone();
        let holder = std::thread::spawn(move || {
            let txn =
                thegn_core::util::begin_git_mutation(&worktree, std::time::Duration::from_secs(5))
                    .unwrap();
            ready_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            drop(txn);
        });
        ready_rx.recv().unwrap();

        let error =
            stash_unstaged_within(&loc, "unstaged only", std::time::Duration::from_millis(100))
                .unwrap_err();
        assert!(error.to_string().contains("mutation lock"), "{error:#}");
        assert_eq!(repo.head(), head, "no temp commit was made");
        assert!(CliGit.stash_list(&loc).unwrap().is_empty());

        release_tx.send(()).unwrap();
        holder.join().unwrap();
    }

    #[test]
    fn stash_unstaged_reports_a_failed_index_restore() {
        let repo = TestRepo::new("st-unstaged-restore-fails");
        ident(&repo.dir);
        repo.commit_file("f.txt", "base\n", "c0");
        std::fs::write(repo.dir.join("f.txt"), "dirty\n").unwrap();
        let loc = repo.loc();
        let head_lock = repo.dir.join(".git/HEAD.lock");
        let hook_lock = head_lock.clone();
        UNSTAGED_STASH_STEP_HOOK.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move |step| {
                if step == "restore" {
                    std::fs::write(&hook_lock, b"held by test").unwrap();
                }
            }));
        });

        let result =
            stash_unstaged_within(&loc, "unstaged only", std::time::Duration::from_millis(100));
        UNSTAGED_STASH_STEP_HOOK.with(|hook| *hook.borrow_mut() = None);
        std::fs::remove_file(&head_lock).unwrap();

        let error = result.unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("restoring the index"), "{message}");
        assert!(message.contains("[thegn] temp index"), "{message}");
        assert_ne!(
            repo.head(),
            repo.sha_of("c0"),
            "temp commit remains on HEAD"
        );
        assert_eq!(CliGit.stash_list(&loc).unwrap().len(), 1);
    }

    #[test]
    fn stash_show_includes_untracked_files() {
        let repo = TestRepo::new("st-show");
        ident(&repo.dir);
        repo.commit_file("f.txt", "one\n", "c0");
        std::fs::write(repo.dir.join("f.txt"), "two\n").unwrap();
        std::fs::write(repo.dir.join("brand-new.txt"), "fresh\n").unwrap();
        let loc = repo.loc();
        CliGit.stash_push(&loc, "with untracked", true).unwrap();

        let patch = CliGit.stash_show(&loc, "stash@{0}").unwrap();
        assert!(patch.contains("f.txt"), "{patch}");
        assert!(
            patch.contains("brand-new.txt") && patch.contains("+fresh"),
            "-u must surface the untracked third parent: {patch}"
        );
    }

    #[test]
    fn stash_show_by_sha_survives_index_shift() {
        let repo = TestRepo::new("st-sha");
        ident(&repo.dir);
        repo.commit_file("f.txt", "base\n", "c0");
        std::fs::write(repo.dir.join("f.txt"), "stash A\n").unwrap();
        let loc = repo.loc();
        CliGit.stash_push(&loc, "stash A", false).unwrap();
        let sha_a = repo.out(&["rev-parse", "stash@{0}"]).trim().to_string();
        std::fs::write(repo.dir.join("f.txt"), "stash B\n").unwrap();
        CliGit.stash_push(&loc, "stash B", false).unwrap();

        let patch = CliGit.stash_show(&loc, &sha_a).unwrap();
        assert!(patch.contains("stash A"), "{patch}");
        assert!(!patch.contains("stash B"), "{patch}");
    }

    #[test]
    fn stash_show_never_runs_textconv_or_external_diff() {
        let repo = TestRepo::new("st-show-textconv");
        ident(&repo.dir);
        std::fs::write(repo.dir.join("sample.txt"), "before\n").unwrap();
        std::fs::write(repo.dir.join(".gitattributes"), "*.txt diff=canary\n").unwrap();
        git_in(&repo.dir, &["add", "sample.txt", ".gitattributes"]);
        git_in(&repo.dir, &["commit", "-q", "-m", "initial"]);
        std::fs::write(repo.dir.join("sample.txt"), "stashed content\n").unwrap();
        let loc = repo.loc();
        CliGit.stash_push(&loc, "textconv test", false).unwrap();
        let sha = repo.out(&["rev-parse", "stash@{0}"]).trim().to_string();

        let marker = repo.dir.join("textconv-ran");
        let driver = repo.dir.join("textconv.sh");
        std::fs::write(
            &driver,
            format!("#!/bin/sh\ntouch '{}'\ncat \"$1\"\n", marker.display()),
        )
        .unwrap();
        git_in(
            &repo.dir,
            &[
                "config",
                "diff.canary.textconv",
                &format!("sh '{}'", driver.display()),
            ],
        );

        let plain_patch = repo.out(&["stash", "show", "-p", "--textconv", &sha]);
        assert!(marker.exists(), "explicit textconv is the positive control");
        assert!(plain_patch.contains("stashed content"), "{plain_patch}");
        std::fs::remove_file(&marker).unwrap();

        let patch = CliGit.stash_show(&loc, &sha).unwrap();
        assert!(patch.contains("stashed content"), "{patch}");
        assert!(!marker.exists(), "stash display must disable textconv");
    }

    #[test]
    fn stash_show_does_not_wait_for_the_mutation_lock() {
        let repo = TestRepo::new("st-show-no-lock");
        ident(&repo.dir);
        repo.commit_file("f.txt", "before\n", "initial");
        std::fs::write(repo.dir.join("f.txt"), "stashed\n").unwrap();
        let loc = repo.loc();
        CliGit.stash_push(&loc, "read test", false).unwrap();
        let sha = repo.out(&["rev-parse", "stash@{0}"]).trim().to_string();
        let lock_path = thegn_core::util::git_common_dir(&repo.dir).join("thegn-git.lock");
        if lock_path.exists() {
            std::fs::remove_file(&lock_path).unwrap();
        }
        std::fs::create_dir(lock_path).unwrap();

        let patch = CliGit.stash_show(&loc, &sha).unwrap();

        assert!(patch.contains("stashed"), "{patch}");
    }

    #[test]
    fn stash_drop_by_sha_survives_index_shift() {
        let repo = TestRepo::new("st-drop-shift");
        ident(&repo.dir);
        repo.commit_file("f.txt", "base\n", "c0");
        let loc = repo.loc();
        std::fs::write(repo.dir.join("f.txt"), "stash A\n").unwrap();
        CliGit.stash_push(&loc, "stash A", false).unwrap();
        let sha_a = CliGit.stash_list(&loc).unwrap()[0].sha.clone();
        std::fs::write(repo.dir.join("f.txt"), "stash B\n").unwrap();
        CliGit.stash_push(&loc, "stash B", false).unwrap();
        let sha_b = CliGit.stash_list(&loc).unwrap()[0].sha.clone();

        CliGit.stash_drop(&loc, &sha_a).unwrap();

        let remaining = CliGit.stash_list(&loc).unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].sha, sha_b, "drop removed the selected stash A");
    }

    #[test]
    fn stash_pop_and_apply_by_sha_target_the_shifted_entry() {
        let apply_repo = TestRepo::new("st-apply-shift");
        ident(&apply_repo.dir);
        apply_repo.commit_file("f.txt", "base\n", "c0");
        let apply_loc = apply_repo.loc();
        std::fs::write(apply_repo.dir.join("f.txt"), "stash A\n").unwrap();
        CliGit.stash_push(&apply_loc, "stash A", false).unwrap();
        let apply_sha_a = CliGit.stash_list(&apply_loc).unwrap()[0].sha.clone();
        std::fs::write(apply_repo.dir.join("f.txt"), "stash B\n").unwrap();
        CliGit.stash_push(&apply_loc, "stash B", false).unwrap();
        let apply_sha_b = CliGit.stash_list(&apply_loc).unwrap()[0].sha.clone();

        CliGit.stash_apply(&apply_loc, &apply_sha_a).unwrap();

        assert_eq!(read(&apply_repo, "f.txt"), "stash A\n");
        let apply_stashes = CliGit.stash_list(&apply_loc).unwrap();
        assert_eq!(apply_stashes.len(), 2, "apply preserves both entries");
        assert!(apply_stashes.iter().any(|entry| entry.sha == apply_sha_b));

        let pop_repo = TestRepo::new("st-pop-shift");
        ident(&pop_repo.dir);
        pop_repo.commit_file("f.txt", "base\n", "c0");
        let pop_loc = pop_repo.loc();
        std::fs::write(pop_repo.dir.join("f.txt"), "stash A\n").unwrap();
        CliGit.stash_push(&pop_loc, "stash A", false).unwrap();
        let pop_sha_a = CliGit.stash_list(&pop_loc).unwrap()[0].sha.clone();
        std::fs::write(pop_repo.dir.join("f.txt"), "stash B\n").unwrap();
        CliGit.stash_push(&pop_loc, "stash B", false).unwrap();
        let pop_sha_b = CliGit.stash_list(&pop_loc).unwrap()[0].sha.clone();

        CliGit.stash_pop(&pop_loc, &pop_sha_a).unwrap();

        assert_eq!(read(&pop_repo, "f.txt"), "stash A\n");
        let pop_stashes = CliGit.stash_list(&pop_loc).unwrap();
        assert_eq!(pop_stashes.len(), 1);
        assert_eq!(
            pop_stashes[0].sha, pop_sha_b,
            "pop removed selected stash A"
        );
    }

    #[test]
    fn stash_mutation_refuses_a_vanished_sha() {
        let repo = TestRepo::new("st-vanished");
        ident(&repo.dir);
        repo.commit_file("f.txt", "base\n", "c0");
        let loc = repo.loc();
        std::fs::write(repo.dir.join("f.txt"), "stash A\n").unwrap();
        CliGit.stash_push(&loc, "stash A", false).unwrap();
        let sha_a = CliGit.stash_list(&loc).unwrap()[0].sha.clone();
        std::fs::write(repo.dir.join("f.txt"), "stash B\n").unwrap();
        CliGit.stash_push(&loc, "stash B", false).unwrap();
        let sha_b = CliGit.stash_list(&loc).unwrap()[0].sha.clone();
        CliGit.stash_drop(&loc, &sha_a).unwrap();

        for result in [
            CliGit.stash_drop(&loc, &sha_a),
            CliGit.stash_pop(&loc, &sha_a),
            CliGit.stash_apply(&loc, &sha_a),
        ] {
            let error = result.unwrap_err().to_string();
            assert!(error.contains("no longer exists"), "{error}");
        }
        let remaining = CliGit.stash_list(&loc).unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].sha, sha_b);
    }

    #[test]
    fn stash_mutation_refuses_when_the_mutation_lock_is_unavailable() {
        let repo = TestRepo::new("st-no-lock");
        ident(&repo.dir);
        repo.commit_file("f.txt", "base\n", "c0");
        let loc = repo.loc();
        std::fs::write(repo.dir.join("f.txt"), "stash A\n").unwrap();
        CliGit.stash_push(&loc, "stash A", false).unwrap();
        let sha_a = CliGit.stash_list(&loc).unwrap()[0].sha.clone();
        let lock_path = thegn_core::util::git_common_dir(&repo.dir).join("thegn-git.lock");
        // The earlier stash push left the regular lock file behind; replace it
        // with a directory so the lock cannot be opened.
        if lock_path.exists() {
            std::fs::remove_file(&lock_path).unwrap();
        }
        std::fs::create_dir(lock_path).unwrap();

        let error = CliGit.stash_drop(&loc, &sha_a).unwrap_err().to_string();

        assert!(error.contains("mutation lock"), "{error}");
        let remaining = CliGit.stash_list(&loc).unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].sha, sha_a);
    }
}
