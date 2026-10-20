use super::HOST_GIT_FENCE;
use crate::integrate::{self, DriverMergeOpts};
use std::path::{Path, PathBuf};
use std::time::Duration;
use thegn_core::util;

struct Fixture {
    _env: crate::testenv::IsolatedState,
    root: tempfile::TempDir,
    repo: PathBuf,
    ours: String,
    theirs: String,
    conflict_tree: String,
    sentinel: PathBuf,
}

#[expect(clippy::disallowed_methods, reason = "isolated Git fixture subprocess")]
fn git(dir: &Path, args: &[&str]) -> String {
    let output = util::git_cmd(dir).args(args).output().unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

#[expect(
    clippy::disallowed_methods,
    reason = "make isolated hook fixture executable"
)]
fn make_executable(path: &Path) {
    let status = std::process::Command::new("chmod")
        .arg("+x")
        .arg(path)
        .status()
        .unwrap();
    assert!(status.success(), "chmod failed for {}", path.display());
}

#[expect(
    clippy::disallowed_methods,
    reason = "make isolated fixture directory private"
)]
fn make_private(path: &Path) {
    let status = std::process::Command::new("chmod")
        .args(["700", path.to_str().unwrap()])
        .status()
        .unwrap();
    assert!(status.success());
}

impl Fixture {
    fn new() -> Option<Self> {
        if thegn_core::sandbox_backend::host_os() == thegn_core::sandbox_backend::HostOs::Windows {
            return None;
        }
        let root = tempfile::Builder::new()
            .prefix("thegn-git-fence-")
            .tempdir_in(std::fs::canonicalize(std::env::temp_dir()).unwrap())
            .unwrap();
        make_private(root.path());
        let global = root.path().join("gitconfig");
        let template = root.path().join("template");
        std::fs::write(&global, "").unwrap();
        std::fs::create_dir(&template).unwrap();
        let _env = crate::testenv::IsolatedState::with_env(&[
            ("GIT_CONFIG_GLOBAL", global.to_str().unwrap()),
            ("GIT_CONFIG_NOSYSTEM", "1"),
            ("GIT_CONFIG_COUNT", "0"),
            ("GIT_CONFIG_PARAMETERS", ""),
            ("GIT_TEMPLATE_DIR", template.to_str().unwrap()),
        ]);
        let repo = root.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["config", "user.name", "Git Fence Test"]);
        git(
            &repo,
            &["config", "user.email", "git-fence@example.invalid"],
        );
        std::fs::write(repo.join("conflict.txt"), "base\n").unwrap();
        git(&repo, &["add", "conflict.txt"]);
        git(
            &repo,
            &["-c", "commit.gpgsign=false", "commit", "-qm", "base"],
        );
        let base = git(&repo, &["rev-parse", "HEAD"]);
        git(&repo, &["checkout", "-qb", "feat", &base]);
        std::fs::write(repo.join("conflict.txt"), "theirs\n").unwrap();
        git(
            &repo,
            &["-c", "commit.gpgsign=false", "commit", "-am", "theirs"],
        );
        let theirs = git(&repo, &["rev-parse", "HEAD"]);
        git(&repo, &["checkout", "-q", "main"]);
        std::fs::write(repo.join("conflict.txt"), "ours\n").unwrap();
        git(
            &repo,
            &["-c", "commit.gpgsign=false", "commit", "-am", "ours"],
        );
        let ours = git(&repo, &["rev-parse", "HEAD"]);
        let conflict_tree = git(&repo, &["rev-parse", &format!("{base}^{{tree}}")]);
        let hooks = root.path().join("hooks-path");
        std::fs::create_dir(&hooks).unwrap();
        let sentinel = root.path().join("hook-fired");
        let script = format!("#!/bin/sh\necho fired >> '{}'\n", sentinel.display());
        for hook_dir in [repo.join(".git/hooks"), hooks.clone()] {
            std::fs::create_dir_all(&hook_dir).unwrap();
            for hook in ["post-checkout", "post-merge"] {
                let path = hook_dir.join(hook);
                std::fs::write(&path, &script).unwrap();
                make_executable(&path);
            }
        }
        git(
            &repo,
            &["config", "core.hooksPath", hooks.to_str().unwrap()],
        );
        Some(Self {
            _env,
            root,
            repo,
            ours,
            theirs,
            conflict_tree,
            sentinel,
        })
    }
}

#[test]
fn plain_worktree_add_fires_the_canary() {
    let Some(fixture) = Fixture::new() else {
        return;
    };
    let worktree = fixture.root.path().join("plain-worktree");
    let worktree_arg = worktree.to_str().unwrap();
    assert!(util::git_ok(
        &fixture.repo,
        &["worktree", "add", "--detach", worktree_arg, "HEAD"]
    ));
    assert!(
        std::fs::read_to_string(&fixture.sentinel)
            .unwrap()
            .contains("fired")
    );
    assert!(util::git_ok(
        &fixture.repo,
        &["worktree", "remove", "--force", worktree_arg]
    ));
    std::fs::remove_file(&fixture.sentinel).unwrap();
    git(&fixture.repo, &["config", "--unset", "core.hooksPath"]);
    let default_worktree = fixture.root.path().join("default-hooks-worktree");
    let default_worktree_arg = default_worktree.to_str().unwrap();
    assert!(util::git_ok(
        &fixture.repo,
        &["worktree", "add", "--detach", default_worktree_arg, "HEAD"]
    ));
    assert!(
        std::fs::read_to_string(&fixture.sentinel)
            .unwrap()
            .contains("fired")
    );
    assert!(util::git_ok(
        &fixture.repo,
        &["worktree", "remove", "--force", default_worktree_arg]
    ));
}

#[test]
fn fenced_worktree_add_does_not_fire_hooks() {
    let Some(fixture) = Fixture::new() else {
        return;
    };
    let worktree = fixture.root.path().join("fenced-worktree");
    assert!(super::ok(
        &fixture.repo,
        &[
            "worktree",
            "add",
            "--detach",
            worktree.to_str().unwrap(),
            "HEAD"
        ]
    ));
    assert!(!fixture.sentinel.exists());
}

#[test]
fn driver_merge_does_not_fire_hooks() {
    let Some(fixture) = Fixture::new() else {
        return;
    };
    let result = integrate::driver_merge(
        &fixture.repo,
        &fixture.ours,
        &fixture.theirs,
        &fixture.conflict_tree,
        &["conflict.txt".to_owned()],
        DriverMergeOpts {
            rerere: None,
            timeout: Duration::from_secs(30),
            overrides: None,
        },
    );
    assert!(result.is_none());
    assert!(!fixture.sentinel.exists());
}

#[test]
fn regenerate_merge_does_not_fire_hooks() {
    let Some(fixture) = Fixture::new() else {
        return;
    };
    let result = integrate::regenerate_merge(
        &fixture.repo,
        &fixture.ours,
        &fixture.theirs,
        &["conflict.txt".to_owned()],
        "true",
        Duration::from_secs(30),
        &[],
    );
    assert!(result.is_some());
    assert!(!fixture.sentinel.exists());
}

#[test]
fn fence_args_precede_the_subcommand() {
    let Some(fixture) = Fixture::new() else {
        return;
    };
    let args = super::cmd(&fixture.repo)
        .get_args()
        .map(std::ffi::OsStr::to_owned)
        .collect::<Vec<_>>();
    let expected = ["-C", fixture.repo.to_str().unwrap()]
        .into_iter()
        .chain(HOST_GIT_FENCE)
        .map(std::ffi::OsString::from)
        .collect::<Vec<_>>();
    assert_eq!(args, expected);
}
