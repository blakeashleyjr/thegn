//! Actual automatic cleanup with private entries in the real resource registries.
use super::*;
use crate::agent::cleanup_fixture::RegistryCustody;
use crate::merge_lifecycle::{CleanupOutcome, remove_landed_with_config};
use crate::worktree_lifecycle::teardown_fixture::RuntimeTeardownObservation;
use thegn_core::config::{Config, DataMode};
use thegn_core::db::Db;
use thegn_core::hooks::HookEntry;
use thegn_core::projection::ProjectionSpec;
use thegn_core::store::{WorkspaceStore, WorktreeAuxStore};

fn registry_projection(fixture: &Fixture) -> ProjectionSpec {
    ProjectionSpec {
        mode: DataMode::Sshfs,
        placement: thegn_core::placement::Placement::Ssh(
            thegn_core::placement::SshPlacement::plain(
                "fixture.invalid".into(),
                22,
                false,
                thegn_core::placement::TransportKind::Ssh,
            ),
        ),
        remote_dir: "/private-fixture-not-connected".into(),
        mountpoint: fixture.wt.to_str().unwrap().into(),
    }
}

fn register_landed(fixture: &Fixture) -> (Db, String) {
    let db = Db::open_memory().unwrap();
    let root = fixture.root.to_str().unwrap();
    let path = fixture.wt.to_str().unwrap();
    db.put_workspace(root, "private", "git").unwrap();
    db.put_worktree("feature", root, path, "feature", None, None)
        .unwrap();
    let commit = text(&fixture.root, &["rev-parse", "HEAD"]).unwrap();
    db.enqueue_merge(path, "feature", "main").unwrap();
    db.update_merge_status(path, "landed", Some(&commit), None, None)
        .unwrap();
    (db, commit)
}

fn config_with_markers(fixture: &Fixture) -> (Config, PathBuf, PathBuf) {
    let mut config = Config::default();
    config.sandbox.enabled = false;
    let pre = fixture._dir.path().join("pre-destroy.receipt");
    let post = fixture._dir.path().join("post-destroy.receipt");
    config.hooks.pre_destroy = vec![HookEntry::Command(format!(
        "printf invoked > {}",
        util::sh_quote(pre.to_str().unwrap())
    ))];
    config.hooks.post_destroy = vec![HookEntry::Command(format!(
        "printf invoked > {}",
        util::sh_quote(post.to_str().unwrap())
    ))];
    (config, pre, post)
}

#[test]
fn active_projection_and_sync_refuse_before_hooks_or_explicit_runtime_teardown() {
    for (projection, sync) in [(true, false), (false, true), (true, true)] {
        let _isolation = TestIsolation::new();
        let fixture = Fixture::new();
        let path = fixture.wt.to_str().unwrap();
        let (db, commit) = register_landed(&fixture);
        let (config, pre, post) = config_with_markers(&fixture);
        let before_queue = db.list_merge_queue().unwrap();
        let before_cache = format!("{:?}", db.worktree_record(path).unwrap());
        let before_refs = text(
            &fixture.root,
            &[
                "for-each-ref",
                "--format=%(refname) %(objectname)",
                "refs/heads",
            ],
        )
        .unwrap();
        let before_bytes = std::fs::read(fixture.wt.join("tracked")).unwrap();
        assert_eq!(RegistryCustody::snapshot(&fixture.wt), (None, None));
        let registry = RegistryCustody::install(
            &fixture.wt,
            projection.then(|| registry_projection(&fixture)),
            sync.then(|| "private-provider-env-not-resolved".into()),
        );
        let expected_registry = RegistryCustody::snapshot(&fixture.wt);
        assert_eq!(expected_registry.0.is_some(), projection);
        assert_eq!(expected_registry.1.is_some(), sync);
        let teardown = RuntimeTeardownObservation::install(&fixture.wt);
        let result = remove_landed_with_config(
            &config,
            &db,
            &fixture.root,
            path,
            "feature",
            "main",
            Some(&commit),
            &before_queue[0],
            true,
        );
        let CleanupOutcome::Refused { reason } = result else {
            panic!("attached resource admitted automatic cleanup: {result:?}");
        };
        assert!(reason.contains("projection/provider sync"), "{reason}");
        assert_eq!(teardown.calls(), 0, "explicit teardown boundary entered");
        assert!(
            !pre.exists() && !post.exists(),
            "refusal must precede hooks"
        );
        assert_eq!(db.list_merge_queue().unwrap(), before_queue);
        assert_eq!(
            format!("{:?}", db.worktree_record(path).unwrap()),
            before_cache
        );
        assert_eq!(
            std::fs::read(fixture.wt.join("tracked")).unwrap(),
            before_bytes
        );
        assert_eq!(
            text(
                &fixture.root,
                &[
                    "for-each-ref",
                    "--format=%(refname) %(objectname)",
                    "refs/heads"
                ]
            )
            .unwrap(),
            before_refs
        );
        assert_eq!(RegistryCustody::snapshot(&fixture.wt), expected_registry);
        drop(registry);
        assert_eq!(RegistryCustody::snapshot(&fixture.wt), (None, None));

        // The same private landed selection is now eligible. This proves the
        // resource refusal did not mutate/revoke it and that the production
        // automatic callback does not fall through to ambient teardown.
        let result = remove_landed_with_config(
            &config,
            &db,
            &fixture.root,
            path,
            "feature",
            "main",
            Some(&commit),
            &before_queue[0],
            true,
        );
        assert!(
            matches!(result, CleanupOutcome::Removed { .. }),
            "{result:?}"
        );
        assert!(!fixture.wt.exists());
        assert!(fixture.root.join("tracked").is_file());
        assert_eq!(std::fs::read(&pre).unwrap(), b"invoked");
        assert_eq!(std::fs::read(&post).unwrap(), b"invoked");
        assert_eq!(teardown.calls(), 0);
        assert_eq!(
            text(&fixture.root, &["rev-parse", "refs/heads/feature"]).unwrap(),
            commit
        );
        assert!(db.worktree_record(path).unwrap().is_none());
        assert_eq!(
            db.list_merge_queue().unwrap().len(),
            1,
            "branch cleanup hold retained"
        );
    }
}

#[test]
fn registry_fixture_restores_prior_custody_on_unwind() {
    let _isolation = TestIsolation::new();
    let fixture = Fixture::new();
    assert_eq!(RegistryCustody::snapshot(&fixture.wt), (None, None));
    let outer = RegistryCustody::install(
        &fixture.wt,
        Some(registry_projection(&fixture)),
        Some("prior-private-env".into()),
    );
    let before = RegistryCustody::snapshot(&fixture.wt);
    let result = std::panic::catch_unwind(|| {
        let _inner = RegistryCustody::install(&fixture.wt, None, Some("inner-env".into()));
        panic!("private assertion unwind");
    });
    assert!(result.is_err());
    assert_eq!(RegistryCustody::snapshot(&fixture.wt), before);
    drop(outer);
    assert_eq!(RegistryCustody::snapshot(&fixture.wt), (None, None));
}
