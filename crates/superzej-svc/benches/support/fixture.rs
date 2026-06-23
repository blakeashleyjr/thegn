//! Hermetic git fixture for the perf benches.
//!
//! Builds a throwaway repo with `n` linked worktrees (a configurable subset
//! dirtied) and a bare `origin` so `ahead_behind` resolves an upstream — the
//! same shape `test/perf/lib/fixture.sh` builds for the process-level harness.
//! Keep the two in sync.
//!
//! IMPORTANT: returns `GitLoc::Local` values constructed directly — never via
//! `GitLoc::for_worktree`, which opens the daily superzej DB. `HOME`,
//! `GIT_CONFIG_GLOBAL` and `XDG_STATE_HOME` are pointed at the tempdir so the
//! bench can't read the developer's gitconfig or touch real state (and because
//! `~/.gitconfig` is masked as a *directory* in some sandboxes).

use std::path::Path;
use std::process::Command;
use superzej_core::remote::GitLoc;
use tempfile::TempDir;

/// A built fixture. Hold it for the lifetime of the bench — dropping it removes
/// the tempdir.
pub struct GitFixture {
    /// Owns the tempdir; never read directly — held so its `Drop` cleans up when
    /// the fixture goes out of scope.
    #[allow(dead_code)]
    pub dir: TempDir,
    pub worktrees: Vec<GitLoc>,
}

fn git(dir: &Path, args: &[&str], home: &Path, gitconfig: &Path) {
    let status = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("HOME", home)
        .env("GIT_CONFIG_GLOBAL", gitconfig)
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .status()
        .expect("spawn git");
    assert!(status.success(), "git {:?} failed", args);
}

/// Build a fixture with `n` worktrees, `dirty` of them carrying an uncommitted
/// file. Returns the fixture (hold it alive) — use `.worktrees` for the locs.
pub fn build(n: usize, dirty: usize) -> GitFixture {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let home = root.join("home");
    let gitconfig = root.join("gitconfig");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        &gitconfig,
        "[user]\n\tname = bench\n\temail = bench@example.invalid\n[init]\n\tdefaultBranch = main\n",
    )
    .unwrap();
    // Belt-and-suspenders: keep any DB access (shouldn't happen) off real state.
    unsafe {
        std::env::set_var("XDG_STATE_HOME", root.join("state"));
    }

    let repo = root.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"], &home, &gitconfig);
    let src = repo.join("src");
    std::fs::create_dir_all(&src).unwrap();
    for i in 0..20 {
        std::fs::write(repo.join(format!("file_{i}.txt")), format!("line {i}\n")).unwrap();
        std::fs::write(src.join(format!("mod_{i}.rs")), format!("fn f{i}() {{}}\n")).unwrap();
    }
    git(&repo, &["add", "-A"], &home, &gitconfig);
    git(
        &repo,
        &["-c", "commit.gpgsign=false", "commit", "-q", "-m", "seed"],
        &home,
        &gitconfig,
    );

    // Bare origin so ahead/behind has an upstream.
    let origin = root.join("origin.git");
    git(
        root,
        &[
            "clone",
            "-q",
            "--bare",
            repo.to_str().unwrap(),
            origin.to_str().unwrap(),
        ],
        &home,
        &gitconfig,
    );
    git(
        &repo,
        &["remote", "add", "origin", origin.to_str().unwrap()],
        &home,
        &gitconfig,
    );
    git(&repo, &["fetch", "-q", "origin"], &home, &gitconfig);

    let wt_root = root.join("worktrees");
    std::fs::create_dir_all(&wt_root).unwrap();
    let mut worktrees = Vec::with_capacity(n);
    for i in 0..n {
        let wt = wt_root.join(format!("wt-{i}"));
        git(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                &format!("wt-{i}"),
                wt.to_str().unwrap(),
                "main",
            ],
            &home,
            &gitconfig,
        );
        // Track origin/main so ahead/behind does real work.
        git(
            &wt,
            &[
                "branch",
                "-q",
                "--set-upstream-to=origin/main",
                &format!("wt-{i}"),
            ],
            &home,
            &gitconfig,
        );
        if i < dirty {
            std::fs::write(wt.join("UNCOMMITTED.txt"), "scratch\n").unwrap();
        }
        // Constructed directly — never GitLoc::for_worktree (opens the DB).
        worktrees.push(GitLoc::Local(wt));
    }

    GitFixture { dir, worktrees }
}
