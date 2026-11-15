//! Worktree directories with a newline or trailing space in their name must
//! pair with their branch exactly through `git worktree list --porcelain -z`
//! (THE-376). Invalid-UTF-8 directory names are unsupported (needs THE-368).

use std::path::Path;

use thegn_core::testenv::require_or_skip;
use thegn_core::util::git_cmd;
use thegn_core::util::worktree_branches;
use thegn_core::worktree::BranchSet;

fn git_available() -> bool {
    git_cmd(Path::new("."))
        .arg("--version")
        .output()
        .is_ok_and(|out| out.status.success())
}

fn git(dir: &Path, args: &[&str]) {
    let out = git_cmd(dir)
        .args([
            "-c",
            "commit.gpgsign=false",
            "-c",
            "user.name=tester",
            "-c",
            "user.email=tester@example.invalid",
            "-c",
            "core.quotepath=true",
        ])
        .args(args)
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn worktree_paths_with_newline_and_trailing_space_pair_with_their_branch() {
    if !require_or_skip("git on PATH", git_available()) {
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let base = tmp.path().canonicalize().expect("canonicalize");
    let repo = base.join("repo");
    std::fs::create_dir(&repo).expect("mkdir repo");
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("seed.txt"), "seed\n").expect("seed");
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "seed"]);

    let cases = [
        ("wt\nnewline", "feat-newline"),
        ("wt trailing ", "feat-trailing"),
    ];
    let mut expected: Vec<(String, Option<String>)> = vec![(
        repo.to_string_lossy().into_owned(),
        Some("main".to_string()),
    )];
    for (dir, branch) in cases {
        let path = base.join(dir);
        // Probe the filesystem first: a refused name skips instead of failing.
        let accepted = std::fs::create_dir(&path).is_ok();
        if !require_or_skip(&format!("filesystem accepts directory {dir:?}"), accepted) {
            continue;
        }
        std::fs::remove_dir(&path).expect("rmdir probe");
        let path_str = path.to_string_lossy().into_owned();
        git(&repo, &["worktree", "add", "-q", "-b", branch, &path_str]);
        expected.push((path_str, Some(branch.to_string())));
    }
    expected.sort();

    let mut listed = worktree_branches(&repo).expect("worktree listing parses");
    listed.sort();
    assert_eq!(
        listed, expected,
        "no split at the newline, no trimmed space"
    );

    let set = BranchSet::load_checked(&repo).expect("load_checked");
    for (_, branch) in &expected {
        let branch = branch.as_deref().expect("every worktree has a branch");
        assert!(set.taken(branch), "{branch} is taken");
    }
    assert!(!set.taken("unrelated"), "unrelated branch is free");
}

#[test]
fn load_checked_errs_outside_a_git_dir() {
    if !require_or_skip("git on PATH", git_available()) {
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    match BranchSet::load_checked(tmp.path()) {
        Ok(_) => panic!("load_checked must refuse outside a git dir"),
        Err(error) => assert!(error.contains("could not list branches"), "{error}"),
    }
}
