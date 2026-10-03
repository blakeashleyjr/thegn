//! Line-level staging: construct partial patches from the diff the user is
//! looking at (thegn-core's `patch::transform`) and pipe them to
//! `git apply --cached [--reverse]`. The diff text MUST come from the
//! sanitized invocations on `GitBackend` (`unstaged_diff`/`staged_diff`) so
//! the constructed patch matches what `git apply` expects; if the worktree
//! changed underneath the cached diff, the apply fails loudly and the caller
//! re-hydrates (lazygit behavior).

use super::{GitBackend, run_stdin, run_w};
use anyhow::{Context, Result, anyhow};
use thegn_core::patch::{Selection, parse_patch, transform};
use thegn_core::remote::GitLoc;

/// Build the partial patch for `sel` over the (single-file) `diff` text.
fn partial(diff: &str, sel: &Selection, reverse: bool) -> Result<String> {
    let files = parse_patch(diff);
    let file = files
        .first()
        .ok_or_else(|| anyhow!("no diff to stage (file changed underneath?)"))?;
    transform(file, sel, reverse).ok_or_else(|| anyhow!("no lines selected"))
}

pub trait StageOps: GitBackend {
    /// Stage the selected lines of an unstaged diff
    /// (`git apply --cached -`).
    fn stage_lines(&self, loc: &GitLoc, diff: &str, sel: &Selection) -> Result<()> {
        let patch = partial(diff, sel, false)?;
        run_stdin(
            loc,
            &[],
            &["apply", "--cached", "--whitespace=nowarn", "-"],
            patch.as_bytes(),
        )
        .map(|_| ())
    }

    /// Unstage the selected lines of a staged diff
    /// (`git apply --cached --reverse -`).
    fn unstage_lines(&self, loc: &GitLoc, diff: &str, sel: &Selection) -> Result<()> {
        let patch = partial(diff, sel, true)?;
        run_stdin(
            loc,
            &[],
            &["apply", "--cached", "--reverse", "--whitespace=nowarn", "-"],
            patch.as_bytes(),
        )
        .map(|_| ())
    }

    /// Discard the selected lines from the working tree
    /// (`git apply --reverse -`). Destructive — confirm at the call site.
    fn discard_lines(&self, loc: &GitLoc, diff: &str, sel: &Selection) -> Result<()> {
        let patch = partial(diff, sel, true)?;
        run_stdin(
            loc,
            &[],
            &["apply", "--reverse", "--whitespace=nowarn", "-"],
            patch.as_bytes(),
        )
        .map(|_| ())
    }

    /// Record an untracked file in the index without content
    /// (`git add --intent-to-add`), making it line-stageable.
    fn intent_to_add(&self, loc: &GitLoc, path: &str) -> Result<()> {
        run_w(loc, &[], &["add", "--intent-to-add", "--", path]).map(|_| ())
    }

    /// Discard a whole file: restore **from HEAD** for tracked content
    /// (`checkout HEAD --`, which resets both the index and the worktree),
    /// `clean -f` for untracked. Destructive — confirm at the call site.
    ///
    /// `checkout HEAD --`, NOT the bare `checkout --`: the bare form restores
    /// from the INDEX, so discarding a file whose bad edit was already staged
    /// copied the staged content back over itself — the confirm said
    /// "unrecoverable", the op reported success, and nothing changed.
    fn discard_file(&self, loc: &GitLoc, path: &str, untracked: bool) -> Result<()> {
        if untracked {
            return run_w(
                loc,
                &[("GIT_LITERAL_PATHSPECS", "1")],
                &["clean", "-f", "--", path],
            )
            .map(|_| ());
        }
        // A staged-NEW file has no HEAD version to restore — discarding it
        // means removing it (index + worktree), same net effect as `clean` on
        // its untracked form. Probe HEAD first so a genuine checkout failure
        // is never "recovered" by deleting the file.
        // Only a successful read that finds nothing selects removal; any
        // execution/repository/object error refuses (fail closed). `ls-tree`
        // exits 0 with empty output for an absent path, unlike `cat-file -e`
        // whose exit 128 is shared by a missing path and a broken repository.
        // Every path argument below is a LITERAL file name, but `rm`, `checkout`
        // and `clean` expand pathspec globs by default: discarding a file named
        // `*.txt` would otherwise delete/restore every `*.txt`.
        const LIT: &[(&str, &str)] = &[("GIT_LITERAL_PATHSPECS", "1")];
        let head_ok = run_w(
            loc,
            LIT,
            &["rev-parse", "--verify", "--quiet", "HEAD^{commit}"],
        );
        let in_head = match head_ok {
            Ok(_) => !run_w(loc, LIT, &["ls-tree", "-z", "HEAD", "--", path])
                .context("could not read HEAD tree; refusing discard")?
                .is_empty(),
            Err(e) => {
                // Unborn HEAD (fresh repo): HEAD names a branch whose ref does
                // not exist yet, so nothing is in HEAD. Any other failure
                // still refuses.
                let unborn = run_w(loc, LIT, &["symbolic-ref", "-q", "HEAD"])
                    .ok()
                    .map(|t| t.trim().to_string())
                    .filter(|t| !t.is_empty())
                    .and_then(|target| {
                        run_w(loc, LIT, &["for-each-ref", "--format=%(refname)", &target])
                            .ok()
                            .map(|o| !o.lines().any(|l| l.trim() == target))
                    })
                    .unwrap_or(false);
                if !unborn {
                    return Err(e.context("HEAD unreadable; refusing discard"));
                }
                false
            }
        };
        if in_head {
            run_w(loc, LIT, &["checkout", "HEAD", "--", path]).map(|_| ())
        } else {
            run_w(loc, LIT, &["rm", "-f", "--", path]).map(|_| ())
        }
    }

    /// Stage everything (`git add -A`).
    fn stage_all(&self, loc: &GitLoc) -> Result<()> {
        run_w(loc, &[], &["add", "-A"]).map(|_| ())
    }

    /// Unstage everything (`git reset -q HEAD`).
    fn unstage_all(&self, loc: &GitLoc) -> Result<()> {
        run_w(loc, &[], &["reset", "-q", "HEAD"]).map(|_| ())
    }
}

impl<T: GitBackend + ?Sized> StageOps for T {}

#[cfg(test)]
mod tests {
    use super::super::testutil::{TestRepo, commit_empty, git_in};
    use super::super::{CliGit, GitBackend};
    use super::StageOps;
    use thegn_core::patch::{LineKind, Selection, parse_patch};

    /// Selection over `parse_patch(diff)[0]` of every Add/Del line for which
    /// `pred(kind, text)` holds (context lines are never selectable changes).
    fn select_changes(diff: &str, pred: impl Fn(LineKind, &str) -> bool) -> Selection {
        let files = parse_patch(diff);
        let file = files.first().expect("diff parses to one file");
        let mut sel = Selection::default();
        for (hi, h) in file.hunks.iter().enumerate() {
            for (li, l) in h.lines.iter().enumerate() {
                if matches!(l.kind, LineKind::Add | LineKind::Del) && pred(l.kind, &l.text) {
                    sel.insert(hi, li);
                }
            }
        }
        sel
    }

    /// A 30-line file, `line 1` … `line 30`.
    fn numbered(n: usize) -> String {
        (1..=n).map(|i| format!("line {i}\n")).collect()
    }

    /// The same file with the lines in `changed` rewritten to `CHANGED{i}` —
    /// spaced so -U3 contexts produce multiple hunks (some shared).
    fn with_changes(n: usize, changed: &[usize]) -> String {
        (1..=n)
            .map(|i| {
                if changed.contains(&i) {
                    format!("CHANGED{i}\n")
                } else {
                    format!("line {i}\n")
                }
            })
            .collect()
    }

    fn has_line(haystack: &str, line: &str) -> bool {
        haystack.lines().any(|l| l == line)
    }

    #[test]
    fn stage_two_of_five_changed_lines() {
        let repo = TestRepo::new("stage-partial");
        repo.commit_file("f.txt", &numbered(30), "base");
        // 5 changes: lines 3 | 11+15 | 23+27 — three -U3 hunks.
        let changed = [3, 11, 15, 23, 27];
        std::fs::write(repo.dir.join("f.txt"), with_changes(30, &changed)).unwrap();
        let loc = repo.loc();

        let diff = CliGit.unstaged_diff(&loc, "f.txt").unwrap();
        assert!(parse_patch(&diff)[0].hunks.len() > 1, "want multiple hunks");
        let sel = select_changes(&diff, |k, t| {
            k == LineKind::Add && (t == "CHANGED3" || t == "CHANGED15")
        });
        assert_eq!(sel.len(), 2);
        CliGit.stage_lines(&loc, &diff, &sel).unwrap();

        // Exactly the two selected additions are cached.
        let cached = repo.out(&["diff", "--cached"]);
        assert!(has_line(&cached, "+CHANGED3"), "{cached}");
        assert!(has_line(&cached, "+CHANGED15"), "{cached}");
        for i in [11, 23, 27] {
            assert!(!cached.contains(&format!("CHANGED{i}")), "{cached}");
        }
        // The working tree still carries all 5 changes; the unstaged diff
        // still shows the 3 we did not stage.
        let on_disk = std::fs::read_to_string(repo.dir.join("f.txt")).unwrap();
        for i in changed {
            assert!(on_disk.contains(&format!("CHANGED{i}")));
        }
        let unstaged = repo.out(&["diff"]);
        for i in [11, 23, 27] {
            assert!(has_line(&unstaged, &format!("+CHANGED{i}")), "{unstaged}");
        }
    }

    #[test]
    fn stage_every_line_equals_git_add() {
        let lines = TestRepo::new("stage-all-lines");
        let control = TestRepo::new("stage-all-control");
        for repo in [&lines, &control] {
            repo.commit_file("f.txt", &numbered(30), "base");
            std::fs::write(
                repo.dir.join("f.txt"),
                with_changes(30, &[3, 11, 15, 23, 27]),
            )
            .unwrap();
        }

        let diff = CliGit.unstaged_diff(&lines.loc(), "f.txt").unwrap();
        let sel = select_changes(&diff, |_, _| true);
        assert_eq!(sel.len(), 10, "5 dels + 5 adds");
        CliGit.stage_lines(&lines.loc(), &diff, &sel).unwrap();
        git_in(&control.dir, &["add", "f.txt"]);

        let staged = lines.out(&["diff", "--cached"]);
        assert!(!staged.is_empty());
        assert_eq!(staged, control.out(&["diff", "--cached"]));
        // Nothing left unstaged either way.
        assert!(lines.out(&["diff"]).is_empty());
    }

    #[test]
    fn unstage_one_line_moves_it_back_to_unstaged() {
        let repo = TestRepo::new("unstage-line");
        repo.commit_file("f.txt", "one\ntwo\n", "base");
        std::fs::write(repo.dir.join("f.txt"), "one\nNEW\ntwo\n").unwrap();
        git_in(&repo.dir, &["add", "f.txt"]);
        let loc = repo.loc();

        let staged = CliGit.staged_diff(&loc, "f.txt").unwrap();
        let sel = select_changes(&staged, |k, t| k == LineKind::Add && t == "NEW");
        assert_eq!(sel.len(), 1);
        CliGit.unstage_lines(&loc, &staged, &sel).unwrap();

        assert!(repo.out(&["diff", "--cached"]).is_empty());
        assert!(has_line(&repo.out(&["diff"]), "+NEW"));
    }

    #[test]
    fn discard_one_of_two_changes() {
        let repo = TestRepo::new("discard-lines");
        repo.commit_file("f.txt", "a\nb\nc\nd\ne\nf\ng\nh\n", "base");
        std::fs::write(repo.dir.join("f.txt"), "a\nB\nc\nd\ne\nF\ng\nh\n").unwrap();
        let loc = repo.loc();

        let diff = CliGit.unstaged_diff(&loc, "f.txt").unwrap();
        // The whole first change: its del AND its add.
        let sel = select_changes(&diff, |_, t| t == "b" || t == "B");
        assert_eq!(sel.len(), 2);
        CliGit.discard_lines(&loc, &diff, &sel).unwrap();

        assert_eq!(
            std::fs::read_to_string(repo.dir.join("f.txt")).unwrap(),
            "a\nb\nc\nd\ne\nF\ng\nh\n",
            "first change reverted, second kept"
        );
    }

    #[test]
    fn crlf_file_line_stages_cleanly() {
        let repo = TestRepo::new("stage-crlf");
        repo.commit_file("dos.txt", "one\r\ntwo\r\nthree\r\n", "base");
        std::fs::write(repo.dir.join("dos.txt"), "one\r\nTWO\r\nthree\r\n").unwrap();
        let loc = repo.loc();

        let diff = CliGit.unstaged_diff(&loc, "dos.txt").unwrap();
        // PatchLine text keeps the trailing \r — selecting by it proves the
        // parse did not strip CRLF.
        let sel = select_changes(&diff, |_, t| t == "two\r" || t == "TWO\r");
        assert_eq!(sel.len(), 2, "CRLF text must keep \\r:\n{diff:?}");
        CliGit.stage_lines(&loc, &diff, &sel).unwrap();

        let cached = repo.out(&["diff", "--cached"]);
        assert!(cached.lines().any(|l| l.starts_with("+TWO")), "{cached:?}");
        assert!(repo.out(&["diff"]).is_empty(), "fully staged");
    }

    #[test]
    fn no_trailing_newline_stages_with_marker() {
        let repo = TestRepo::new("stage-noeol");
        repo.commit_file("n.txt", "alpha\nbeta\nomega", "base");
        std::fs::write(repo.dir.join("n.txt"), "alpha\nbeta\nOMEGA").unwrap();
        let loc = repo.loc();

        let diff = CliGit.unstaged_diff(&loc, "n.txt").unwrap();
        let sel = select_changes(&diff, |_, t| t == "omega" || t == "OMEGA");
        assert_eq!(sel.len(), 2);
        CliGit.stage_lines(&loc, &diff, &sel).unwrap();

        let cached = repo.out(&["diff", "--cached"]);
        assert!(has_line(&cached, "+OMEGA"), "{cached}");
        assert!(cached.contains("\\ No newline at end of file"), "{cached}");
        assert!(repo.out(&["diff"]).is_empty());
    }

    #[test]
    fn intent_to_add_makes_an_untracked_file_line_stageable() {
        let repo = TestRepo::new("stage-ita");
        commit_empty(&repo.dir, "base"); // a HEAD to diff --cached against
        std::fs::write(repo.dir.join("new.txt"), "a\nb\nc\n").unwrap();
        let loc = repo.loc();

        // Untracked → no unstaged diff; intent-to-add exposes it as all-adds.
        assert!(CliGit.unstaged_diff(&loc, "new.txt").unwrap().is_empty());
        CliGit.intent_to_add(&loc, "new.txt").unwrap();
        let diff = CliGit.unstaged_diff(&loc, "new.txt").unwrap();
        let sel = select_changes(&diff, |k, t| k == LineKind::Add && t == "b");
        assert_eq!(sel.len(), 1, "{diff}");
        CliGit.stage_lines(&loc, &diff, &sel).unwrap();

        let cached = repo.out(&["diff", "--cached", "--", "new.txt"]);
        assert!(has_line(&cached, "+b"), "{cached}");
        assert!(!has_line(&cached, "+a"), "{cached}");
        assert!(!has_line(&cached, "+c"), "{cached}");
    }

    #[test]
    fn discard_file_restores_tracked_and_removes_untracked() {
        let repo = TestRepo::new("discard-file");
        repo.commit_file("t.txt", "original\n", "base");
        std::fs::write(repo.dir.join("t.txt"), "mangled\n").unwrap();
        std::fs::write(repo.dir.join("u.txt"), "scratch\n").unwrap();
        let loc = repo.loc();

        CliGit.discard_file(&loc, "t.txt", false).unwrap();
        assert_eq!(
            std::fs::read_to_string(repo.dir.join("t.txt")).unwrap(),
            "original\n"
        );
        CliGit.discard_file(&loc, "u.txt", true).unwrap();
        assert!(!repo.dir.join("u.txt").exists());
        assert!(CliGit.status(&loc).unwrap().is_empty());
    }

    #[test]
    fn discard_file_refuses_when_head_is_unreadable() {
        let repo = TestRepo::new("discard-unknown");
        repo.commit_file("t.txt", "original\n", "base");
        std::fs::write(repo.dir.join("t.txt"), "precious\n").unwrap();
        // Break HEAD's tree object: the probe now errors instead of reporting
        // "absent", which used to select `git rm -f` and delete the file.
        let tree = repo.out(&["rev-parse", "HEAD^{tree}"]);
        let tree = tree.trim();
        let obj = repo
            .dir
            .join(".git/objects")
            .join(&tree[..2])
            .join(&tree[2..]);
        let mut perm = std::fs::metadata(&obj).unwrap().permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        perm.set_readonly(false);
        std::fs::set_permissions(&obj, perm).unwrap();
        std::fs::remove_file(&obj).unwrap();
        let loc = repo.loc();
        assert!(CliGit.discard_file(&loc, "t.txt", false).is_err());
        assert_eq!(
            std::fs::read_to_string(repo.dir.join("t.txt")).unwrap(),
            "precious\n"
        );
    }

    #[test]
    fn discard_file_removes_a_staged_new_file() {
        let repo = TestRepo::new("discard-staged-new");
        repo.commit_file("t.txt", "x\n", "base");
        std::fs::write(repo.dir.join("n.txt"), "new\n").unwrap();
        repo.out(&["add", "n.txt"]);
        let loc = repo.loc();
        CliGit.discard_file(&loc, "n.txt", false).unwrap();
        assert!(!repo.dir.join("n.txt").exists());
        assert!(CliGit.status(&loc).unwrap().is_empty());
    }

    #[test]
    fn discard_file_treats_the_path_literally() {
        let repo = TestRepo::new("discard-glob");
        repo.commit_file("a.txt", "orig\n", "base");
        std::fs::write(repo.dir.join("a.txt"), "edited\n").unwrap();
        std::fs::write(repo.dir.join("*.txt"), "new\n").unwrap();
        repo.out(&["add", "--", "*.txt"]);
        let loc = repo.loc();
        CliGit.discard_file(&loc, "*.txt", false).unwrap();
        assert!(!repo.dir.join("*.txt").exists());
        assert_eq!(
            std::fs::read_to_string(repo.dir.join("a.txt")).unwrap(),
            "edited\n",
            "a glob must not discard other files"
        );
    }

    #[test]
    fn discard_untracked_file_treats_the_path_literally() {
        let repo = TestRepo::new("discard-glob-untracked");
        repo.commit_file("a.txt", "orig\n", "base");
        std::fs::write(repo.dir.join("keep.txt"), "keep\n").unwrap();
        std::fs::write(repo.dir.join("*.txt"), "new\n").unwrap();
        let loc = repo.loc();
        CliGit.discard_file(&loc, "*.txt", true).unwrap();
        assert!(!repo.dir.join("*.txt").exists());
        assert!(repo.dir.join("keep.txt").exists());
    }

    #[test]
    fn discard_file_in_an_unborn_repo_removes_the_staged_file() {
        let repo = TestRepo::new("discard-unborn");
        std::fs::write(repo.dir.join("n.txt"), "new\n").unwrap();
        repo.out(&["add", "n.txt"]);
        let loc = repo.loc();
        CliGit.discard_file(&loc, "n.txt", false).unwrap();
        assert!(!repo.dir.join("n.txt").exists());
    }

    #[test]
    fn discard_file_still_refuses_when_head_is_unreadable() {
        let repo = TestRepo::new("discard-badhead");
        repo.commit_file("t.txt", "x\n", "base");
        std::fs::write(repo.dir.join("t.txt"), "precious\n").unwrap();
        let loc = repo.loc();
        std::fs::write(repo.dir.join(".git/HEAD"), "garbage\n").unwrap();
        assert!(CliGit.discard_file(&loc, "t.txt", false).is_err());
        assert_eq!(
            std::fs::read_to_string(repo.dir.join("t.txt")).unwrap(),
            "precious\n"
        );
    }

    #[test]
    fn stage_all_unstage_all_round_trip() {
        let repo = TestRepo::new("stage-roundtrip");
        repo.commit_file("t.txt", "one\n", "base");
        std::fs::write(repo.dir.join("t.txt"), "changed\n").unwrap();
        std::fs::write(repo.dir.join("u.txt"), "new\n").unwrap();
        let loc = repo.loc();

        CliGit.stage_all(&loc).unwrap();
        let st = CliGit.status(&loc).unwrap();
        assert!(st.iter().all(|f| f.unstaged == ' '), "{st:?}");
        assert!(st.iter().any(|f| f.path == "t.txt" && f.staged == 'M'));
        assert!(st.iter().any(|f| f.path == "u.txt" && f.staged == 'A'));

        CliGit.unstage_all(&loc).unwrap();
        let st = CliGit.status(&loc).unwrap();
        assert!(
            st.iter()
                .any(|f| f.path == "t.txt" && f.staged == ' ' && f.unstaged == 'M'),
            "{st:?}"
        );
        assert!(
            st.iter().any(|f| f.path == "u.txt" && f.staged == '?'),
            "{st:?}"
        );
        // Index-only ops never move HEAD or create commits.
        assert_eq!(repo.subjects(), vec!["base"]);
        assert_eq!(repo.head(), repo.sha_of("base"));
    }

    #[test]
    fn stale_diff_fails_loudly() {
        let repo = TestRepo::new("stage-stale");
        repo.commit_file("f.txt", "one\ntwo\nthree\n", "base");
        std::fs::write(repo.dir.join("f.txt"), "one\nTWO\nthree\n").unwrap();
        let loc = repo.loc();
        let stale = CliGit.unstaged_diff(&loc, "f.txt").unwrap();

        // The file (and the index) change underneath the cached diff.
        std::fs::write(repo.dir.join("f.txt"), "one\nZZZ\nthree\n").unwrap();
        git_in(&repo.dir, &["add", "f.txt"]);

        let sel = select_changes(&stale, |_, _| true);
        assert!(
            CliGit.stage_lines(&loc, &stale, &sel).is_err(),
            "stale patch must be rejected, not silently mis-applied"
        );
    }

    #[test]
    fn empty_diff_or_empty_selection_errors() {
        let repo = TestRepo::new("stage-empty");
        repo.commit_file("f.txt", "one\n", "base");
        std::fs::write(repo.dir.join("f.txt"), "ONE\n").unwrap();
        let loc = repo.loc();

        // No diff at all (file unchanged / wrong path).
        assert!(CliGit.stage_lines(&loc, "", &Selection::default()).is_err());
        // A real diff but nothing selected.
        let diff = CliGit.unstaged_diff(&loc, "f.txt").unwrap();
        assert!(
            CliGit
                .stage_lines(&loc, &diff, &Selection::default())
                .is_err()
        );
        // Neither attempt touched the index.
        assert!(repo.out(&["diff", "--cached"]).is_empty());
    }
}
