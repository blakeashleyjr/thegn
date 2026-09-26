use super::*;

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    wt: PathBuf,
}

#[test]
fn local_runtime_admission_pins_workspace_selection() {
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
    db.set_worktree_env(path, "changed-worktree").unwrap();
    assert!(
        settled
            .revalidate(&db, &fixture.root, path)
            .unwrap_err()
            .contains("environment changed")
    );
    db.set_worktree_env(path, "").unwrap();
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
    assert!(oci_resources_absent(Some(dir.path().as_os_str()), dir.path()).is_ok());
    assert!(oci_resources_absent(None, dir.path()).is_err());
    assert!(oci_resources_absent(Some(std::ffi::OsStr::new("relative")), dir.path()).is_err());
    std::fs::write(
        dir.path().join("docker"),
        "not an executable; must still refuse",
    )
    .unwrap();
    assert!(
        oci_resources_absent(Some(dir.path().as_os_str()), dir.path())
            .unwrap_err()
            .contains("docker")
    );
}

#[test]
fn oci_output_parsers_reject_truncation_and_garbage() {
    assert_eq!(
        parse_container_ids(b"0123456789abcdef\n").unwrap(),
        vec!["0123456789abcdef"]
    );
    assert!(parse_container_ids(b"garbage\n").is_err());
    assert!(parse_container_ids(b"0123").is_err());
    assert_eq!(
        parse_mount_sources(b"mount:/tmp/unrelated\nend\n").unwrap(),
        vec![PathBuf::from("/tmp/unrelated")]
    );
    assert!(parse_mount_sources(b"garbage\n").is_err());
    assert!(parse_mount_sources(b"mount:/tmp/unrelated\n").is_err());
}

#[test]
fn oci_inspect_parser_accepts_one_record_per_batched_container() {
    assert_eq!(
        parse_mount_sources(b"mount:/tmp/first\nend\nmount:/tmp/second\nend\n").unwrap(),
        vec![PathBuf::from("/tmp/first"), PathBuf::from("/tmp/second")]
    );
}

/// The two real CLIs disagree about separators, and only one of them was modelled
/// when this parser was written. Captured verbatim from the binaries on a host with
/// two containers: podman's native CLI prints no separator, while docker's
/// docker-compat CLI prints a blank line after every container's `end`. Rejecting
/// that blank refused every worktree on a machine with docker installed — the live
/// symptom this issue exists to remove, which reappeared after the end-marker fix.
/// A blank is accepted ONLY directly after `end`.
#[test]
fn oci_inspect_parser_accepts_the_docker_compat_blank_separator() {
    let podman = b"mount:/a\nmount:/b\nend\nmount:/c\nend\n".as_slice();
    let docker = b"mount:/a\nmount:/b\nend\n\nmount:/c\nend\n\n".as_slice();
    let expected = vec![
        PathBuf::from("/a"),
        PathBuf::from("/b"),
        PathBuf::from("/c"),
    ];
    assert_eq!(parse_mount_sources(podman).unwrap(), expected);
    assert_eq!(parse_mount_sources(docker).unwrap(), expected);
    // A separator is not a licence to skip blanks anywhere else.
    assert!(parse_mount_sources(b"mount:/a\n\nend\n").is_err());
    assert!(parse_mount_sources(b"\nmount:/a\nend\n").is_err());
}

#[test]
fn oci_parsers_reject_blank_records_as_malformed() {
    assert!(parse_container_ids(b"\n").is_err());
    assert!(parse_container_ids(b"0123456789abcdef\n\n").is_err());
    assert!(parse_mount_sources(b"\nend\n").is_err());
    assert!(parse_mount_sources(b"mount:/tmp/source\n\nend\n").is_err());
}

#[test]
fn mount_source_matching_is_canonical_and_path_component_bounded() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("worktree");
    let nested = target.join("nested");
    let sibling = dir.path().join("worktree-copy");
    std::fs::create_dir(&target).unwrap();
    std::fs::create_dir(&nested).unwrap();
    std::fs::create_dir(&sibling).unwrap();
    assert!(mount_source_may_own(&target, &target));
    assert!(mount_source_may_own(&nested, &target));
    assert!(!mount_source_may_own(&sibling, &target));
    assert!(mount_source_may_own(
        &target.join("does-not-exist"),
        &target
    ));
    assert!(mount_source_may_own(
        Path::new("thegn-the690-missing-relative-source"),
        &target
    ));
    #[cfg(unix)]
    {
        let link = dir.path().join("nested-link");
        std::os::unix::fs::symlink(&nested, &link).unwrap();
        assert!(mount_source_may_own(&link, &target));
    }
}

#[cfg(unix)]
#[test]
fn oci_probe_queries_docker_and_podman_once_and_fails_closed() {
    use std::os::unix::fs::PermissionsExt;

    fn runtime(dir: &Path, name: &str, inspect: &str) {
        let script = format!(
            "#!/bin/sh\ncase \"$1\" in\n  ps) printf '%s\\n' 0123456789abcdef ;;\n  inspect) printf '%s' '{}' ;;\n  *) exit 64 ;;\nesac\n",
            inspect
        );
        let path = dir.join(name);
        std::fs::write(&path, script).unwrap();
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(path, permissions).unwrap();
    }

    let target_root = tempfile::tempdir().unwrap();
    let target = target_root.path().join("worktree");
    std::fs::create_dir(&target).unwrap();
    let unrelated = target_root.path().join("unrelated");
    std::fs::create_dir(&unrelated).unwrap();

    let unrelated_bin = tempfile::tempdir().unwrap();
    runtime(
        unrelated_bin.path(),
        "docker",
        &format!("mount:{}\nend\n", unrelated.display()),
    );
    assert!(oci_resources_absent(Some(unrelated_bin.path().as_os_str()), &target).is_ok());

    let owned_bin = tempfile::tempdir().unwrap();
    std::fs::create_dir(target.join("nested")).unwrap();
    runtime(
        owned_bin.path(),
        "docker",
        &format!("mount:{}\nend\n", target.join("nested").display()),
    );
    assert!(
        oci_resources_absent(Some(owned_bin.path().as_os_str()), &target)
            .unwrap_err()
            .contains("may own the worktree")
    );

    let malformed_bin = tempfile::tempdir().unwrap();
    runtime(malformed_bin.path(), "docker", "garbage\n");
    assert!(
        oci_resources_absent(Some(malformed_bin.path().as_os_str()), &target)
            .unwrap_err()
            .contains("could not be queried")
    );

    let unknown_bin = tempfile::tempdir().unwrap();
    runtime(unknown_bin.path(), "container", "end\n");
    assert!(
        oci_resources_absent(Some(unknown_bin.path().as_os_str()), &target)
            .unwrap_err()
            .contains("container")
    );

    let podman_bin = tempfile::tempdir().unwrap();
    let calls = podman_bin.path().join("calls");
    let script = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$1\" >> '{}'\ncase \"$1\" in\n  ps) printf '%s\\n' 0123456789abcdef ;;\n  inspect) printf 'end\\n' ;;\n  *) exit 64 ;;\nesac\n",
        calls.display()
    );
    let podman = podman_bin.path().join("podman");
    std::fs::write(&podman, script).unwrap();
    let mut permissions = std::fs::metadata(&podman).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&podman, permissions).unwrap();
    assert!(oci_resources_absent(Some(podman_bin.path().as_os_str()), &target).is_ok());
    assert_eq!(std::fs::read_to_string(calls).unwrap().lines().count(), 2);
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
fn tracked_and_untracked_status_never_mean_clean_but_ignored_only_is_admissible() {
    let _isolation = TestIsolation::new();
    for path in ["tracked", "untracked"] {
        let fixture = Fixture::new();
        std::fs::write(fixture.wt.join(path), "new user work\n").unwrap();
        assert!(matches!(fixture.probe(), Err(Refusal::Dirty)), "{path}");
        assert_eq!(
            std::fs::read_to_string(fixture.wt.join(path)).unwrap(),
            "new user work\n"
        );
    }
    let fixture = Fixture::new();
    std::fs::write(fixture.wt.join("ignored"), "build output\n").unwrap();
    let verified = fixture
        .probe()
        .expect("ignored-only build state is safely removable");
    assert!(verified.discarded_build_state());
    assert!(fixture.wt.join("ignored").exists());
    let fixture = Fixture::new();
    let index = PathBuf::from(text(&fixture.wt, &["rev-parse", "--git-path", "index"]).unwrap());
    std::fs::write(&index, "corrupt fixture index").unwrap();
    assert!(fixture.probe().is_err());
    assert!(fixture.wt.exists());
    assert!(clean(&fixture._dir.path().join("absent")).is_err());
}

#[test]
fn status_observation_accepts_only_well_formed_ignored_records() {
    let empty = observe_status(Vec::new()).unwrap();
    assert!(!empty.ignored_only);
    assert_eq!(empty.bytes, Vec::<u8>::new());

    let ignored = observe_status(b"!! target/\0!! cache/\0".to_vec()).unwrap();
    assert!(ignored.ignored_only);
    assert_eq!(ignored.bytes, b"!! target/\0!! cache/\0");

    // Well-formed records that are not ignored state are real work.
    for status in [
        b" M tracked\0".as_slice(),
        b"?? untracked\0",
        b"R  new\0",
        b"!x unknown\0",
    ] {
        assert!(
            matches!(observe_status(status.to_vec()), Err(Refusal::Dirty)),
            "{:?}",
            String::from_utf8_lossy(status)
        );
    }
    // A record that does not parse is refused as unsafe, never reported to the
    // operator as an edit they made.
    for malformed in [b"!!".as_slice(), b"!! \0", b"!!x\0"] {
        assert!(
            matches!(observe_status(malformed.to_vec()), Err(Refusal::Unsafe(_))),
            "{:?}",
            String::from_utf8_lossy(malformed)
        );
    }
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
    assert!(
        matches!(fixture.probe(), Err(Refusal::Unsafe(reason)) if reason.contains("private")),
        "a driver a tracked path actually selects must still refuse, and name itself"
    );
    assert!(
        !canary.exists(),
        "a cleanup cleanliness probe must not execute filter code"
    );
}

/// THE-685: a filter driver only runs when a `filter=<name>` attribute selects
/// it. Refusing on configuration alone meant one machine-wide `git lfs install`
/// — which writes `filter.lfs.clean`/`.process` into `~/.gitconfig` — disabled
/// merged-worktree cleanup in EVERY repository, including repositories with no
/// LFS content and no `.gitattributes` at all.
#[test]
fn a_configured_but_unused_filter_driver_does_not_block_cleanup() {
    let _isolation = TestIsolation::new();

    // Configured exactly as `git lfs install` leaves it, but nothing selects it.
    let fixture = Fixture::new();
    let canary = fixture._dir.path().join("unused-filter-ran");
    let command = format!(
        "printf unsafe > {}; cat",
        util::sh_quote(canary.to_str().unwrap())
    );
    setup_git(&fixture.wt, &["config", "filter.lfs.clean", &command]);
    setup_git(&fixture.wt, &["config", "filter.lfs.process", &command]);
    assert!(
        fixture.probe().is_ok(),
        "a driver no tracked path selects must not block cleanup"
    );
    assert!(!canary.exists(), "the probe must not execute filter code");

    // Not covered here: an attribute that names an UNCONFIGURED driver. Any
    // `.gitattributes` would have to be committed to avoid tripping the
    // separate cleanliness refusal, and committing on the feature branch makes
    // it unmerged, which `Verified::probe` then refuses for an unrelated
    // reason. The case is safe by construction anyway — the check only ever
    // looks up drivers that are actually configured.
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
    assert!(matches!(verified.remove(), Err(Refusal::Changed)));
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
fn tracked_work_appearing_after_admission_is_dirty_not_merely_changed() {
    let _isolation = TestIsolation::new();
    let fixture = Fixture::new();
    let verified = fixture.probe().unwrap();
    std::fs::write(fixture.wt.join("tracked"), "appeared after admission").unwrap();

    // `Dirty`, not `Changed`: the operator went back to this worktree and edited
    // it, which is the "edited since landing" fact, not a concurrency signal.
    // The distinction is what the sweep reports, so it must not blur.
    assert!(matches!(verified.remove(), Err(Refusal::Dirty)));
    assert!(fixture.wt.join("tracked").exists());
}

#[test]
fn ignored_work_appearing_after_the_last_guard_is_not_removed() {
    let _isolation = TestIsolation::new();
    let fixture = Fixture::new();
    let verified = fixture.probe().unwrap();

    let result = verified.remove_checked(&|| {
        std::fs::write(
            fixture.wt.join("ignored"),
            "appeared after final status check",
        )
        .map_err(|error| error.to_string())
    });

    assert!(matches!(result, Err(Refusal::Changed)), "{result:?}");
    assert!(fixture.wt.join("ignored").exists());
}

/// The status predicate is not the only thing protecting real work: plain
/// `git worktree remove` refuses a worktree with modified or untracked files,
/// and the sweep deliberately does not pass `--force`. That is the backstop for
/// the unavoidable window between the last status read and the removal, so it
/// must keep holding even if the predicate is ever wrong.
#[test]
fn removal_never_forces_so_git_independently_refuses_real_work() {
    let _isolation = TestIsolation::new();
    for (path, expected_kept) in [
        ("tracked", "edited user work"),
        ("untracked", "new user work"),
    ] {
        let fixture = Fixture::new();
        let verified = fixture.probe().unwrap();
        // Write the work only once the guard has run, i.e. past every check the
        // cleanup code performs. Only Git itself can still refuse here.
        let result = verified.remove_checked(&|| {
            std::fs::write(fixture.wt.join(path), expected_kept).map_err(|e| e.to_string())
        });
        assert!(result.is_err(), "{path}: {result:?}");
        assert_eq!(
            std::fs::read_to_string(fixture.wt.join(path)).unwrap(),
            expected_kept,
            "{path} must survive"
        );
        assert!(fixture.wt.exists(), "{path}: worktree must survive");
    }
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

#[path = "merge_cleanup_runtime_tests.rs"]
mod runtime;
