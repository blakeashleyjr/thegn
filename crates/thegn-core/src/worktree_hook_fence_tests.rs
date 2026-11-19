use std::path::{Path, PathBuf};

use crate::{config::Config, fsperm, util};

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    canary: PathBuf,
}

impl Fixture {
    fn new(tag: &str) -> Self {
        let dir = tempfile::Builder::new()
            .prefix(&format!("tg-hook-fence-{tag}-"))
            .tempdir()
            .unwrap();
        let root = dir.path().to_path_buf();
        git(&root, &["init", "-q", "-b", "main"]);
        git(&root, &["config", "user.email", "t@t.t"]);
        git(&root, &["config", "user.name", "t"]);
        git(&root, &["config", "commit.gpgsign", "false"]);
        git(&root, &["commit", "--allow-empty", "-q", "-m", "init"]);

        let hooks = root.join(".git/hooks");
        let canary = root.join("hook-canary");
        install_hooks(&hooks, &canary);
        Self {
            _dir: dir,
            root,
            canary,
        }
    }

    fn cfg(&self) -> Config {
        Config {
            worktrees_dir: self.root.join(".wt").to_string_lossy().into_owned(),
            ..Default::default()
        }
    }

    fn clear_canary(&self) {
        let _ = std::fs::remove_file(&self.canary);
    }

    fn assert_no_hook(&self) {
        assert!(
            !self.canary.exists(),
            "Git hook unexpectedly wrote the canary"
        );
    }
}

fn git(root: &Path, args: &[&str]) {
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

fn shell_quote(value: &Path) -> String {
    format!("'{}'", value.to_string_lossy().replace('\'', "'\\''"))
}

fn install_hooks(hooks: &Path, canary: &Path) {
    std::fs::create_dir_all(hooks).unwrap();
    let canary = shell_quote(canary);
    for name in ["post-checkout", "reference-transaction"] {
        let path = hooks.join(name);
        std::fs::write(
            &path,
            format!("#!/bin/sh\nprintf '%s\\n' {name} >> {canary}\n"),
        )
        .unwrap();
        fsperm::make_executable_for_test(&path).unwrap();
    }
}

#[test]
fn plain_git_worktree_add_runs_hook_positive_control() {
    if !cfg!(unix) {
        return;
    }
    let fixture = Fixture::new("positive");
    let path = fixture.root.join("plain");
    let path = path.to_str().unwrap();
    git(
        &fixture.root,
        &["worktree", "add", "-q", "-b", "plain", path, "main"],
    );
    assert!(fixture.canary.exists(), "plain Git did not run the hook");
}

#[test]
fn host_worktree_lifecycle_mutations_do_not_run_hooks() {
    if !cfg!(unix) {
        return;
    }
    let fixture = Fixture::new("lifecycle");
    let cfg = fixture.cfg();
    let configured_hooks = fixture.root.join("configured-hooks");
    install_hooks(&configured_hooks, &fixture.canary);
    let configured_hooks_s = configured_hooks.to_str().unwrap();
    git(
        &fixture.root,
        &["config", "core.hooksPath", configured_hooks_s],
    );

    let added = fixture.root.join("added");
    super::add_checked(&fixture.root, "added", "main", &added, &cfg).unwrap();
    fixture.assert_no_hook();
    assert!(super::remove(&fixture.root, &added, "added", false));
    fixture.assert_no_hook();

    git(&fixture.root, &["branch", "delete-me", "main"]);
    fixture.clear_canary();
    super::delete_branch(&fixture.root, "delete-me").unwrap();
    fixture.assert_no_hook();

    let old_path = fixture.root.join("old");
    super::add_checked(&fixture.root, "old", "main", &old_path, &cfg).unwrap();
    fixture.assert_no_hook();
    let new_path = super::rename(&fixture.root, &old_path, "old", "new", &cfg).unwrap();
    assert_ne!(new_path, old_path);
    fixture.assert_no_hook();
}

#[test]
fn host_worktree_add_ignores_repository_hooks_path() {
    if !cfg!(unix) {
        return;
    }
    let fixture = Fixture::new("hooks-path");
    let configured_hooks = fixture.root.join("configured-hooks");
    install_hooks(&configured_hooks, &fixture.canary);
    let configured_hooks_s = configured_hooks.to_str().unwrap();
    git(
        &fixture.root,
        &["config", "core.hooksPath", configured_hooks_s],
    );

    let plain = fixture.root.join("plain");
    let plain_s = plain.to_str().unwrap();
    git(
        &fixture.root,
        &["worktree", "add", "-q", "-b", "plain", plain_s, "main"],
    );
    assert!(
        fixture.canary.exists(),
        "configured hooks path was not used"
    );
    fixture.clear_canary();
    assert!(super::remove(&fixture.root, &plain, "plain", false));
    fixture.assert_no_hook();

    let cfg = fixture.cfg();
    let fenced = fixture.root.join("fenced");
    super::add_checked(&fixture.root, "fenced", "main", &fenced, &cfg).unwrap();
    fixture.assert_no_hook();
}
