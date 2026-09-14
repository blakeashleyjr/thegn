use super::*;

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    wt: PathBuf,
}

#[test]
fn local_runtime_admission_pins_workspace_selection_and_refuses_persisted_session() {
    use thegn_core::store::WorkspaceStore;
    let _isolation = TestIsolation::new();
    let fixture = Fixture::new();
    let db = thegn_core::db::Db::open_memory().unwrap();
    let root = fixture.root.to_str().unwrap();
    let path = fixture.wt.to_str().unwrap();
    db.put_workspace(root, "private", "git").unwrap();
    db.put_worktree("feature", root, path, "feature", None, None)
        .unwrap();
    let mut cfg = thegn_core::config::Config::default();
    cfg.sandbox.enabled = false;
    let settled = LocalResources::settle(&cfg, &db, &fixture.root, path).unwrap();
    db.set_workspace_env(root, "changed").unwrap();
    assert!(
        settled
            .revalidate(&db, &fixture.root, path)
            .unwrap_err()
            .contains("environment changed")
    );
    db.set_workspace_env(root, "").unwrap();
    db.put_tab_group(
        "private-session",
        &thegn_core::models::TabGroupRow {
            name: "private/feature".into(),
            kind: "branch".into(),
            worktree: path.into(),
            ordinal: 0,
            active_tab: 0,
        },
    )
    .unwrap();
    assert!(
        settled
            .revalidate(&db, &fixture.root, path)
            .unwrap_err()
            .contains("runtime/session")
    );
    assert!(fixture.wt.exists());
}

#[test]
fn local_runtime_admission_refuses_auto_and_unknown_environment() {
    use thegn_core::store::WorkspaceStore;
    let _isolation = TestIsolation::new();
    let fixture = Fixture::new();
    let db = thegn_core::db::Db::open_memory().unwrap();
    let root = fixture.root.to_str().unwrap();
    let path = fixture.wt.to_str().unwrap();
    let mut cfg = thegn_core::config::Config::default();
    cfg.sandbox.enabled = true;
    cfg.sandbox.backend = thegn_core::config::SandboxBackend::Auto;
    assert!(LocalResources::settle(&cfg, &db, &fixture.root, path).is_err());
    cfg.sandbox.enabled = false;
    db.put_workspace(root, "private", "git").unwrap();
    db.set_workspace_env(root, "undefined").unwrap();
    assert!(LocalResources::settle(&cfg, &db, &fixture.root, path).is_err());
    assert!(fixture.wt.exists());
}

#[test]
fn oci_discovery_is_bounded_conservative_and_never_executes_candidates() {
    let dir = tempfile::tempdir().unwrap();
    assert!(oci_resources_absent(Some(dir.path().as_os_str())).is_ok());
    assert!(oci_resources_absent(None).is_err());
    assert!(oci_resources_absent(Some(std::ffi::OsStr::new("relative"))).is_err());
    std::fs::write(
        dir.path().join("docker"),
        "not an executable; must still refuse",
    )
    .unwrap();
    assert!(
        oci_resources_absent(Some(dir.path().as_os_str()))
            .unwrap_err()
            .contains("discoverable")
    );
}

#[test]
fn identity_open_rejects_wrong_types_and_symlinks() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("file");
    std::fs::write(&file, "private").unwrap();
    assert!(identity(&file, IdentityKind::Directory).is_err());
    assert!(identity(dir.path(), IdentityKind::File).is_err());
    if crate::platform::test_symlink_supported() {
        let link = dir.path().join("link");
        crate::platform::symlink_file_for_test(&file, &link).unwrap();
        assert!(identity(&link, IdentityKind::File).is_err());
    }
}

#[test]
fn named_pipe_identity_open_is_nonblocking_and_refused() {
    let Some(mkfifo) = util::which_path("mkfifo") else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let fifo = dir.path().join("private-fifo");
    let mut command = std::process::Command::new(mkfifo);
    command.arg(&fifo);
    crate::bounded_git_probe::capture(command, None, "private FIFO creation").unwrap();
    let started = std::time::Instant::now();
    assert!(identity(&fifo, IdentityKind::File).is_err());
    assert!(identity(&fifo, IdentityKind::Directory).is_err());
    assert!(started.elapsed() < std::time::Duration::from_secs(1));
}

#[expect(clippy::disallowed_methods)]
fn setup_git(root: &Path, args: &[&str]) {
    let mut command = util::git_cmd(root);
    TEST_GIT_CONFIG.with(|slot| {
        command
            .env(
                "GIT_CONFIG_GLOBAL",
                slot.borrow().as_ref().expect("private config"),
            )
            .env("GIT_CONFIG_NOSYSTEM", "1");
        command
            .env_remove("GIT_CONFIG_COUNT")
            .env_remove("GIT_CONFIG_PARAMETERS")
            .env_remove("GIT_TEMPLATE_DIR");
    });
    let out = command
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let root = base.join("repo");
        let wt = base.join("feature");
        std::fs::create_dir(&root).unwrap();
        setup_git(&root, &["init", "-q", "-b", "main"]);
        setup_git(&root, &["config", "user.name", "private cleanup fixture"]);
        setup_git(&root, &["config", "user.email", "fixture@example.invalid"]);
        std::fs::write(root.join("tracked"), "base\n").unwrap();
        std::fs::write(root.join(".gitignore"), "ignored\n").unwrap();
        setup_git(&root, &["add", "--", "tracked", ".gitignore"]);
        setup_git(&root, &["commit", "-q", "-m", "base"]);
        setup_git(
            &root,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "feature",
                wt.to_str().unwrap(),
                "main",
            ],
        );
        Self {
            _dir: dir,
            root,
            wt,
        }
    }

    fn probe(&self) -> Result<Verified, Refusal> {
        Verified::probe(
            &self.root,
            self.wt.to_str().unwrap(),
            "feature",
            "main",
            None,
        )
    }
}

#[test]
fn rejects_foreign_main_target_mismatched_and_unregistered_paths() {
    let _isolation = TestIsolation::new();
    let fixture = Fixture::new();
    let foreign = Fixture::new();
    let probe = |path: &Path, branch| {
        Verified::probe(&fixture.root, path.to_str().unwrap(), branch, "main", None)
    };
    assert!(fixture.probe().is_ok());
    assert!(probe(&fixture.root, "feature").is_err());
    assert!(probe(&fixture.wt, "main").is_err());
    assert!(probe(&fixture.wt, "wrong").is_err());
    assert!(probe(&foreign.wt, "feature").is_err());
    let unknown = fixture._dir.path().join("unknown");
    std::fs::create_dir(&unknown).unwrap();
    std::fs::write(unknown.join("keep"), "owned by user").unwrap();
    assert!(probe(&unknown, "feature").is_err());
    assert_eq!(
        std::fs::read_to_string(unknown.join("keep")).unwrap(),
        "owned by user"
    );
    assert!(fixture.root.exists() && fixture.wt.exists() && foreign.wt.exists());
}

#[test]
fn dirty_ignored_untracked_and_unknown_status_never_mean_clean() {
    let _isolation = TestIsolation::new();
    for path in ["tracked", "untracked", "ignored"] {
        let fixture = Fixture::new();
        std::fs::write(fixture.wt.join(path), "new user work\n").unwrap();
        assert!(matches!(fixture.probe(), Err(Refusal::Dirty)), "{path}");
        assert_eq!(
            std::fs::read_to_string(fixture.wt.join(path)).unwrap(),
            "new user work\n"
        );
    }
    let fixture = Fixture::new();
    let index = PathBuf::from(text(&fixture.wt, &["rev-parse", "--git-path", "index"]).unwrap());
    std::fs::write(&index, "corrupt fixture index").unwrap();
    assert!(fixture.probe().is_err());
    assert!(fixture.wt.exists());
    assert!(clean(&fixture._dir.path().join("absent")).is_err());
}

#[test]
fn hidden_index_modifications_and_filters_refuse_without_running_driver() {
    let _isolation = TestIsolation::new();
    for flag in ["--assume-unchanged", "--skip-worktree"] {
        let fixture = Fixture::new();
        setup_git(&fixture.wt, &["update-index", flag, "tracked"]);
        std::fs::write(fixture.wt.join("tracked"), "hidden user work").unwrap();
        assert!(fixture.probe().is_err());
    }
    let fixture = Fixture::new();
    let canary = fixture._dir.path().join("filter-ran");
    let command = format!(
        "printf unsafe > {}; cat",
        util::sh_quote(canary.to_str().unwrap())
    );
    setup_git(&fixture.wt, &["config", "filter.private.clean", &command]);
    std::fs::write(
        fixture.wt.join(".gitattributes"),
        "tracked filter=private\n",
    )
    .unwrap();
    std::fs::write(fixture.wt.join("tracked"), "would invoke clean\n").unwrap();
    assert!(matches!(fixture.probe(), Err(Refusal::Unsafe(reason)) if reason.contains("filters")));
    assert!(
        !canary.exists(),
        "a cleanup cleanliness probe must not execute filter code"
    );
}

#[test]
fn branch_advanced_after_landing_or_during_cleanup_is_preserved() {
    let _isolation = TestIsolation::new();
    let fixture = Fixture::new();
    let verified = fixture.probe().unwrap();
    std::fs::write(fixture.wt.join("tracked"), "new committed work\n").unwrap();
    setup_git(&fixture.wt, &["add", "tracked"]);
    setup_git(&fixture.wt, &["commit", "-q", "-m", "not landed"]);
    assert!(fixture.probe().is_err());
    assert!(verified.remove().is_err());
    assert!(fixture.wt.exists());
    assert_eq!(
        std::fs::read_to_string(fixture.wt.join("tracked")).unwrap(),
        "new committed work\n"
    );
}

#[test]
fn final_validation_preserves_new_ignored_files_and_replaced_directory() {
    let _isolation = TestIsolation::new();
    let fixture = Fixture::new();
    let verified = fixture.probe().unwrap();
    std::fs::write(
        fixture.wt.join("ignored"),
        "appeared after first validation",
    )
    .unwrap();
    assert!(matches!(verified.remove(), Err(Refusal::Dirty)));
    assert!(fixture.wt.join("ignored").exists());
    let fixture = Fixture::new();
    let verified = fixture.probe().unwrap();
    let moved = fixture._dir.path().join("moved");
    std::fs::rename(&fixture.wt, &moved).unwrap();
    std::fs::create_dir(&fixture.wt).unwrap();
    std::fs::write(fixture.wt.join("keep"), "replacement").unwrap();
    assert!(verified.remove().is_err());
    assert!(moved.join("tracked").exists() && fixture.wt.join("keep").exists());
}

#[test]
fn locked_worktree_busy_repository_and_symlink_alias_are_refused() {
    let _isolation = TestIsolation::new();
    let fixture = Fixture::new();
    let verified = fixture.probe().unwrap();
    let lock = util::lock_git_mutations(&fixture.root).unwrap();
    assert!(
        verified.remove().is_err(),
        "busy lock must refuse, never wait indefinitely"
    );
    drop(lock);
    setup_git(
        &fixture.root,
        &["worktree", "lock", fixture.wt.to_str().unwrap()],
    );
    assert!(fixture.probe().is_err());
    if crate::platform::test_symlink_supported() {
        let alias = fixture._dir.path().join("alias");
        crate::platform::test_symlink(&fixture.wt, &alias).unwrap();
        assert!(
            Verified::probe(
                &fixture.root,
                alias.to_str().unwrap(),
                "feature",
                "main",
                None
            )
            .is_err()
        );
    }
    assert!(fixture.wt.exists());
}

#[test]
fn symlinked_mutation_lock_and_revoked_final_guard_preserve_files() {
    let _isolation = TestIsolation::new();
    let fixture = Fixture::new();
    let verified = fixture.probe().unwrap();
    let refusal =
        verified.remove_checked(&|| Err("eligibility revoked immediately before removal".into()));
    assert!(
        matches!(refusal, Err(Refusal::Unsafe(reason)) if reason.contains("eligibility revoked"))
    );
    assert!(fixture.wt.join("tracked").exists());
    if crate::platform::test_symlink_supported() {
        let fixture = Fixture::new();
        let verified = fixture.probe().unwrap();
        let outside = fixture._dir.path().join("keep-lock-target");
        std::fs::write(&outside, "unchanged").unwrap();
        crate::platform::test_symlink(&outside, &verified.common.join("thegn-git.lock")).unwrap();
        assert!(verified.remove().is_err());
        assert_eq!(std::fs::read_to_string(outside).unwrap(), "unchanged");
        assert!(fixture.wt.exists());
    }
}

#[test]
fn actual_no_force_removal_preserves_source_and_target_refs() {
    let _isolation = TestIsolation::new();
    let fixture = Fixture::new();
    let verified = fixture.probe().unwrap();
    verified.remove().unwrap();
    verified.verify_removed().unwrap();
    assert!(!fixture.wt.exists());
    assert!(fixture.root.join("tracked").exists());
    assert_eq!(
        oid(&fixture.root, "refs/heads/feature").unwrap(),
        verified.head
    );
    assert!(oid(&fixture.root, "refs/heads/main").is_ok());
}

#[test]
fn branch_recheckout_after_removal_is_kept() {
    let _isolation = TestIsolation::new();
    let fixture = Fixture::new();
    let verified = fixture.probe().unwrap();
    verified.remove().unwrap();
    let replacement = fixture._dir.path().join("new-checkout");
    setup_git(
        &fixture.root,
        &[
            "worktree",
            "add",
            "-q",
            replacement.to_str().unwrap(),
            "feature",
        ],
    );
    assert!(replacement.join("tracked").exists());
    assert!(oid(&fixture.root, "refs/heads/feature").is_ok());
}

#[test]
fn private_git_transaction_rejects_a_changed_target_oid() {
    let _isolation = TestIsolation::new();
    let fixture = Fixture::new();
    let verified = fixture.probe().unwrap();
    verified.remove().unwrap();
    let old_target = oid(&fixture.root, "refs/heads/main").unwrap();
    std::fs::write(fixture.root.join("tracked"), "target moved\n").unwrap();
    setup_git(&fixture.root, &["add", "tracked"]);
    setup_git(&fixture.root, &["commit", "-q", "-m", "target move"]);
    let transaction = format!(
        "start\nverify refs/heads/main {old_target}\ndelete refs/heads/feature {}\nprepare\ncommit\n",
        verified.head
    );
    assert!(git_input(&fixture.root, &["update-ref", "--stdin"], Some(transaction)).is_err());
    assert_eq!(
        oid(&fixture.root, "refs/heads/feature").unwrap(),
        verified.head
    );
}
