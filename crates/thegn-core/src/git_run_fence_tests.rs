use std::{
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
    time::Duration,
};

use crate::{fsperm, util};

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    canary: PathBuf,
}

impl Fixture {
    fn new(tag: &str) -> Self {
        let dir = tempfile::Builder::new()
            .prefix(&format!("tg-fsmonitor-fence-{tag}-"))
            .tempdir()
            .unwrap();
        let root = dir.path().to_path_buf();
        setup_git(&root, &["init", "-q", "-b", "main"]);
        setup_git(&root, &["config", "user.email", "t@t.t"]);
        setup_git(&root, &["config", "user.name", "t"]);
        setup_git(&root, &["config", "commit.gpgsign", "false"]);
        setup_git(&root, &["commit", "--allow-empty", "-q", "-m", "init"]);

        let canary = root.join("fsmonitor-canary");
        let script = root.join("fsmonitor.sh");
        std::fs::write(
            &script,
            format!("#!/bin/sh\nprintf x >> {}\n", shell_quote(&canary)),
        )
        .unwrap();
        fsperm::make_executable_for_test(&script).unwrap();
        setup_git(
            &root,
            &["config", "core.fsmonitor", script.to_str().unwrap()],
        );

        Self {
            _dir: dir,
            root,
            canary,
        }
    }
}

fn setup_git(root: &Path, args: &[&str]) {
    let mut command = util::git_cmd(root);
    command
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1");
    assert!(
        command.status().unwrap().success(),
        "git command failed: git {}",
        args.join(" ")
    );
}

fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
}

fn policy() -> super::GitPolicy {
    super::GitPolicy::read(Duration::from_secs(10))
}

#[test]
fn plain_git_status_runs_the_fsmonitor_program() {
    if !cfg!(unix) {
        return;
    }
    let fixture = Fixture::new("plain");
    // Not `util::git_cmd`: that helper fences fsmonitor off (THE-225), and this
    // test is the positive control proving plain Git would have run it.
    let status = std::process::Command::new("git") // plain-git-control: THE-225 positive control
        .arg("-C")
        .arg(&fixture.root)
        .args(["diff-index", "--quiet", "HEAD", "--"])
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .status()
        .unwrap();
    assert!(status.success(), "plain Git status failed: {status}");
    assert!(
        fixture.canary.exists(),
        "plain Git did not run the configured fsmonitor"
    );
}

#[test]
fn git_run_never_runs_the_fsmonitor_program() {
    if !cfg!(unix) {
        return;
    }
    let fixture = Fixture::new("git-run");
    let outcome = super::git_run(
        &fixture.root,
        &["diff-index", "--quiet", "HEAD", "--"],
        &policy(),
        &AtomicBool::new(false),
    );
    assert!(outcome.is_success(), "git_run failed: {outcome:?}");
    assert!(!fixture.canary.exists());
}

#[test]
fn util_git_ok_never_runs_the_fsmonitor_program() {
    if !cfg!(unix) {
        return;
    }
    let fixture = Fixture::new("util-git-ok");
    assert!(util::git_ok(
        &fixture.root,
        &["diff-index", "--quiet", "HEAD", "--"]
    ));
    assert!(!fixture.canary.exists());
}
