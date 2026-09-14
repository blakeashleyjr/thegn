use super::*;

fn supported() -> bool {
    thegn_core::sandbox_backend::host_os() != thegn_core::sandbox_backend::HostOs::Windows
}

#[test]
fn ambient_history_overrides_are_rejected_without_disclosing_values() {
    for key in HISTORY_ENV {
        for value in ["", "private-credential-canary"] {
            let error = environment(|asked| (asked == *key).then(|| value.into())).unwrap_err();
            assert!(!error.to_string().contains("private-credential-canary"));
        }
    }
    assert!(environment(|_| None).is_ok());
    assert!(environment(|key| (key == "GIT_NO_REPLACE_OBJECTS").then(|| "1".into())).is_ok());
    assert!(environment(|key| (key == "GIT_NO_REPLACE_OBJECTS").then(|| "0".into())).is_err());
}

#[test]
fn nonunicode_history_override_is_not_erased() {
    if let Some(value) = crate::platform::gate_path::history_test_nonunicode() {
        assert!(environment(|key| (key == "GIT_GRAFT_FILE").then(|| value.clone())).is_err());
    }
}

#[test]
fn absence_is_not_a_symlink_or_special_file_admission() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("grafts");
    absent(&path).unwrap();
    std::fs::write(&path, "").unwrap();
    assert!(absent(&path).is_err());
    if supported() {
        let link = root.path().join("dangling");
        crate::platform::gate_path::history_test_symlink(&root.path().join("missing"), &link)
            .unwrap();
        assert!(absent(&link).is_err());
    }
}

#[test]
fn nonlocal_transport_is_refused_without_executing_its_command() {
    let provider = GitLoc::Provider {
        control_prefix: vec!["must-not-execute-private-canary".into()],
        path: "/not-local".into(),
    };
    assert!(
        CanonicalHistory::local(&provider)
            .err()
            .unwrap()
            .to_string()
            .contains("unsupported")
    );
    let remote = GitLoc::Remote {
        ssh: thegn_core::remote::SshTarget::plain("example.invalid".into(), 22, false),
        path: "/not-local".into(),
    };
    assert!(
        CanonicalHistory::local(&remote)
            .err()
            .unwrap()
            .to_string()
            .contains("unsupported")
    );
}

#[test]
fn canonical_history_platform_without_pins_is_explicitly_unsupported() {
    if supported() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    assert!(CanonicalHistory::capture(root.path()).is_err());
}

struct Fixture {
    _env: crate::testenv::EnvVarGuard,
    root: tempfile::TempDir,
    repo: PathBuf,
    first: String,
    second: String,
}

impl Fixture {
    fn new() -> Option<Self> {
        Self::with_external_graft(false)
    }

    fn with_external_graft(external_graft: bool) -> Option<Self> {
        let root = tempfile::Builder::new()
            .prefix("thegn-history-")
            .tempdir_in(std::fs::canonicalize(std::env::temp_dir()).unwrap())
            .unwrap();
        let global = root.path().join("gitconfig");
        std::fs::write(&global, "").unwrap();
        let template = root.path().join("template");
        std::fs::create_dir(&template).unwrap();
        let state = root.path().join("state");
        let config = root.path().join("config");
        let local = root.path().join("local");
        let graft = root.path().join("external-grafts");
        let mut variables = vec![
            ("XDG_STATE_HOME", state.to_str().unwrap()),
            ("XDG_CONFIG_HOME", config.to_str().unwrap()),
            ("LOCALAPPDATA", local.to_str().unwrap()),
            ("THEGN_DIR", root.path().to_str().unwrap()),
            ("THEGN_PROFILE", ""),
            ("GIT_CONFIG_GLOBAL", global.to_str().unwrap()),
            ("GIT_CONFIG_NOSYSTEM", "1"),
            ("GIT_CONFIG_COUNT", "0"),
            ("GIT_CONFIG_PARAMETERS", ""),
            ("GIT_TEMPLATE_DIR", template.to_str().unwrap()),
        ];
        if external_graft {
            std::fs::write(&graft, "").unwrap();
            variables.push(("GIT_GRAFT_FILE", graft.to_str().unwrap()));
        }
        // Set the process environment before any canonical-history probe can
        // start a worker. The fixture guard restores it on all exits.
        let env = crate::testenv::EnvVarGuard::set(&variables);
        let repo = root.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        let mut value = Self {
            _env: env,
            root,
            repo,
            first: String::new(),
            second: String::new(),
        };
        value.git(&["init", "-q", "-b", "main"]);
        std::fs::create_dir_all(value.repo.join(".git/info")).unwrap();
        value.git(&["config", "user.name", "Private History"]);
        value.git(&["config", "user.email", "private@example.invalid"]);
        value.git(&["config", "commit.gpgsign", "false"]);
        value.git(&["config", "core.hooksPath", template.to_str().unwrap()]);
        std::fs::write(value.repo.join("value"), "one\n").unwrap();
        value.git(&["add", "value"]);
        value.git(&["commit", "-qm", "first"]);
        value.first = value.git(&["rev-parse", "HEAD"]);
        std::fs::write(value.repo.join("value"), "two\n").unwrap();
        value.git(&["commit", "-qam", "second"]);
        value.second = value.git(&["rev-parse", "HEAD"]);
        thegn_core::db::Db::open().unwrap(); // initialize only the owned private registry
        if !supported() {
            let before = value.snapshot();
            assert!(CanonicalHistory::capture(&value.repo).is_err());
            let config = thegn_core::config::MergeQueueConfig {
                gate_on: false,
                gate_command: String::new(),
                ..Default::default()
            };
            assert!(crate::integrate::run_fold(&config, &value.repo, Vec::new()).is_err());
            assert_eq!(value.snapshot(), before);
            return None;
        }
        Some(value)
    }

    #[expect(clippy::disallowed_methods)]
    fn git(&self, args: &[&str]) -> String {
        let output = util::git_cmd(&self.repo).args(args).output().unwrap();
        assert!(output.status.success(), "private Git fixture failed");
        String::from_utf8(output.stdout).unwrap().trim().into()
    }

    fn snapshot(&self) -> (String, Vec<u8>, Vec<u8>, Vec<u8>) {
        (
            self.git(&["show-ref"]),
            std::fs::read(self.repo.join(".git/index")).unwrap(),
            std::fs::read(self.repo.join("value")).unwrap(),
            std::fs::read(self.repo.join(".git/config")).unwrap(),
        )
    }
}

#[test]
fn ordinary_and_linked_repository_history_is_read_only() {
    let Some(fixture) = Fixture::new() else {
        return;
    };
    let before = fixture.snapshot();
    let proof = CanonicalHistory::capture(&fixture.repo).unwrap();
    proof.revalidate().unwrap();
    assert_eq!(fixture.snapshot(), before);
    let linked = fixture.root.path().join("linked");
    fixture.git(&["worktree", "add", "-b", "linked", linked.to_str().unwrap()]);
    let before = fixture.snapshot();
    CanonicalHistory::capture(&linked)
        .unwrap()
        .revalidate()
        .unwrap();
    assert_eq!(fixture.snapshot(), before);
}

#[test]
fn loose_and_packed_replacements_hold_without_changing_any_history() {
    for packed in [false, true] {
        let Some(fixture) = Fixture::new() else {
            return;
        };
        let proof = CanonicalHistory::capture(&fixture.repo).unwrap();
        fixture.git(&["replace", &fixture.first, &fixture.second]);
        if packed {
            fixture.git(&["pack-refs", "--all"]);
        }
        let before = fixture.snapshot();
        assert!(CanonicalHistory::capture(&fixture.repo).is_err());
        assert!(proof.revalidate().is_err());
        assert_eq!(fixture.snapshot(), before);
    }
}

#[test]
fn graft_shallow_and_dangling_metadata_hold_before_and_after_admission() {
    for kind in ["graft", "shallow", "dangling"] {
        let Some(fixture) = Fixture::new() else {
            return;
        };
        let proof = CanonicalHistory::capture(&fixture.repo).unwrap();
        let path = fixture.repo.join(if kind == "shallow" {
            ".git/shallow"
        } else {
            ".git/info/grafts"
        });
        if kind == "dangling" {
            crate::platform::gate_path::history_test_symlink(
                &fixture.root.path().join("missing"),
                &path,
            )
            .unwrap();
        } else {
            std::fs::write(&path, format!("{}\n", fixture.second)).unwrap();
        }
        let before = fixture.snapshot();
        assert!(CanonicalHistory::capture(&fixture.repo).is_err());
        assert!(proof.revalidate().is_err());
        assert_eq!(fixture.snapshot(), before);
        assert!(std::fs::symlink_metadata(path).is_ok());
    }
}

#[test]
fn original_info_directory_identity_is_not_readopted() {
    let Some(fixture) = Fixture::new() else {
        return;
    };
    let proof = CanonicalHistory::capture(&fixture.repo).unwrap();
    let path = fixture.repo.join(".git/info");
    std::fs::rename(&path, fixture.repo.join(".git/old-info")).unwrap();
    std::fs::create_dir(&path).unwrap();
    let before = fixture.snapshot();
    assert!(proof.revalidate().is_err());
    assert_eq!(fixture.snapshot(), before);
}

#[test]
fn callback_history_mutation_overrides_success_and_failure_results() {
    for success in [false, true] {
        let Some(fixture) = Fixture::new() else {
            return;
        };
        let proof = CanonicalHistory::capture(&fixture.repo).unwrap();
        let result: Result<()> = proof.checked(|| {
            std::fs::write(fixture.repo.join(".git/info/grafts"), "").unwrap();
            if success {
                Ok(())
            } else {
                anyhow::bail!("ordinary callback failure")
            }
        });
        assert!(result.unwrap_err().to_string().contains("graft"));
    }
}

#[test]
fn replacement_refuses_fold_and_up_to_date_without_mutation() {
    use crate::integrate::{AttemptOutcome, attempt_land, run_fold};
    let Some(fixture) = Fixture::new() else {
        return;
    };
    fixture.git(&["branch", "candidate", &fixture.second]);
    fixture.git(&["reset", "--hard", &fixture.first]);
    fixture.git(&["replace", &fixture.first, &fixture.second]);
    let before = fixture.snapshot();
    let config = thegn_core::config::MergeQueueConfig {
        sign_commits: false,
        gate_on: false,
        ..Default::default()
    };
    assert!(
        run_fold(
            &config,
            &fixture.repo,
            vec![thegn_core::fold::Branch {
                name: "candidate".into(),
                tip: fixture.second.clone()
            }]
        )
        .is_err()
    );
    assert!(matches!(
        attempt_land(
            &config,
            &fixture.repo,
            "candidate",
            &GitLoc::Local(fixture.repo.clone())
        )
        .unwrap(),
        AttemptOutcome::GateError { .. }
    ));
    assert_eq!(fixture.snapshot(), before);
}

#[test]
fn gate_history_mutation_is_infrastructure_not_blame_or_advancement() {
    use crate::integrate::{GateOutcome, run_fold};
    for gate_exit in [0, 1] {
        let Some(fixture) = Fixture::new() else {
            return;
        };
        fixture.git(&["branch", "candidate", &fixture.second]);
        fixture.git(&["reset", "--hard", &fixture.first]);
        let config = thegn_core::config::MergeQueueConfig {
            sign_commits: false,
            gate_on: true,
            gate_setup_command: String::new(),
            gate_command: format!(
                "git -C {} replace {} {}; exit {gate_exit}",
                util::sh_quote(fixture.repo.to_str().unwrap()),
                fixture.first,
                fixture.second
            ),
            ..Default::default()
        };
        let before = fixture.snapshot();
        let report = run_fold(
            &config,
            &fixture.repo,
            vec![thegn_core::fold::Branch {
                name: "candidate".into(),
                tip: fixture.second.clone(),
            }],
        )
        .unwrap();
        assert!(!report.advanced);
        assert!(report.landed.is_empty());
        assert!(report.deferred.iter().all(|branch| !branch.gate_failed));
        assert!(matches!(report.gate, GateOutcome::Errored { .. }));
        assert_eq!(
            fixture.git(&["rev-parse", "refs/heads/main"]),
            fixture.first
        );
        assert_eq!(
            fixture.git(&["rev-parse", "refs/heads/candidate"]),
            fixture.second
        );
        let after = fixture.snapshot();
        assert_eq!((after.1, after.2, after.3), (before.1, before.2, before.3));
        assert_eq!(
            fixture.git(&["rev-parse", &format!("refs/replace/{}", fixture.first)]),
            fixture.second,
            "authorized gate metadata remains; refusal does not repair it"
        );
    }
}

#[test]
fn history_refusal_preserves_actual_cleanup_queue_and_worktree() {
    cleanup_history_refusal(false);
}

#[test]
fn external_graft_environment_refuses_actual_cleanup_without_default_grafts() {
    cleanup_history_refusal(true);
}

fn cleanup_history_refusal(external_graft: bool) {
    use thegn_core::store::WorktreeAuxStore;
    let Some(fixture) = Fixture::with_external_graft(external_graft) else {
        return;
    };
    let wt = fixture.root.path().join("linked");
    fixture.git(&["worktree", "add", "-b", "candidate", wt.to_str().unwrap()]);
    let db = thegn_core::db::Db::open_memory().unwrap();
    let path = wt.to_str().unwrap();
    db.enqueue_merge(path, "candidate", "main").unwrap();
    db.update_merge_status(path, "landed", Some(&fixture.second), None, None)
        .unwrap();
    let before_rows = db.list_merge_queue().unwrap();
    let before = fixture.snapshot();
    if external_graft {
        assert!(std::fs::symlink_metadata(fixture.repo.join(".git/info/grafts")).is_err());
    } else {
        std::fs::write(fixture.repo.join(".git/info/grafts"), "").unwrap();
    }
    let outcome = crate::merge_lifecycle::remove_landed_with_config(
        &thegn_core::config::Config::default(),
        &db,
        &fixture.repo,
        path,
        "candidate",
        "main",
        Some(&fixture.second),
        &before_rows[0],
        true,
    );
    let crate::merge_lifecycle::CleanupOutcome::Refused { reason } = outcome else {
        panic!("history override must refuse cleanup");
    };
    assert!(reason.contains("canonical history"));
    if external_graft {
        assert!(reason.contains("GIT_GRAFT_FILE"));
        assert!(!reason.contains(fixture.root.path().to_str().unwrap()));
        assert_eq!(
            std::fs::read(fixture.root.path().join("external-grafts")).unwrap(),
            b""
        );
    }
    assert!(wt.is_dir());
    assert_eq!(std::fs::read(wt.join("value")).unwrap(), b"two\n");
    assert_eq!(db.list_merge_queue().unwrap(), before_rows);
    assert_eq!(fixture.snapshot(), before);
}

#[test]
fn strict_registry_accepts_only_documented_local_values_and_pins_observation() {
    let Some(fixture) = Fixture::new() else {
        return;
    };
    let db = Rc::new(Db::open_memory().unwrap());
    let path = fixture.repo.to_str().unwrap();
    let missing = CanonicalHistory::registered(Rc::clone(&db), &fixture.repo).unwrap();
    db.put_workspace(path, "private", "git").unwrap();
    db.put_worktree("main", path, path, "main", None, None)
        .unwrap();
    assert!(
        missing.revalidate().is_err(),
        "new row is not the original missing observation"
    );
    for value in ["", "local"] {
        db.set_worktree_location(path, value).unwrap();
        let history = CanonicalHistory::registered(Rc::clone(&db), &fixture.repo).unwrap();
        history.revalidate().unwrap();
        db.set_worktree_location(path, "{malformed-private-canary")
            .unwrap();
        assert!(history.revalidate().is_err());
    }
    for value in [
        " ",
        "LOCAL",
        "{malformed-private-canary",
        "{\"host\":\"example.invalid\",\"port\":22,\"path\":\"/remote\"}",
    ] {
        db.set_worktree_location(path, value).unwrap();
        let error = CanonicalHistory::registered(Rc::clone(&db), &fixture.repo)
            .err()
            .unwrap();
        assert!(!error.to_string().contains("malformed-private-canary"));
    }
}

#[test]
fn strict_registry_decode_and_query_errors_do_not_become_local_absence() {
    let Some(fixture) = Fixture::new() else {
        return;
    };
    let file = fixture.root.path().join("corrupt-registry.db");
    let db = Rc::new(Db::open_at(&file).unwrap());
    let path = fixture.repo.to_str().unwrap();
    db.put_workspace(path, "private", "git").unwrap();
    db.put_worktree("main", path, path, "main", None, None)
        .unwrap();
    let history = CanonicalHistory::registered(Rc::clone(&db), &fixture.repo).unwrap();
    let writer = rusqlite::Connection::open(&file).unwrap();
    writer
        .execute(
            "UPDATE worktrees SET position='bad', location='remote' WHERE worktree=?1",
            [path],
        )
        .unwrap();
    assert!(history.revalidate().is_err());
    assert!(CanonicalHistory::registered(Rc::clone(&db), &fixture.repo).is_err());
    writer.execute("DROP TABLE worktrees", []).unwrap();
    assert!(history.revalidate().is_err());
    assert!(CanonicalHistory::registered(db, &fixture.repo).is_err());
}
