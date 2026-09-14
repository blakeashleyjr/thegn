use super::*;
use std::cell::Cell;
use std::sync::atomic::Ordering;
use thegn_core::issue::{AgentDispatchStatus, NewDispatch};
use thegn_core::store::{NotificationStore, WorkspaceStore};

use crate::pr_authorship::tests::Fixture;

fn request(fixture: &Fixture) -> Request {
    Request {
        worktree: fixture.dir.path().into(),
        snapshot: PrReviewSnapshot {
            worktree_key: GitLoc::worktree_cache_key(fixture.dir.path()),
            branch: fixture.pr.head_ref_name.clone(),
            pr_number: fixture.pr.number,
            head_oid: fixture.pr.head_ref_oid.clone(),
            ..Default::default()
        },
        command: "fixture-agent {prompt}".into(),
        title: "cached title".into(),
        base: "cached base".into(),
        url: fixture.pr.url.clone(),
        feedback: "synthetic review feedback".into(),
    }
}

fn queue() -> PrQueueConfig {
    PrQueueConfig {
        own_prs_only: true,
        ..Default::default()
    }
}

fn forbid_prepare() -> Result<Option<SandboxSpec>, String> {
    panic!("denied authority reached sandbox preparation")
}

fn forbid_launch(_: &AgentTaskRun<'_>) -> bool {
    panic!("denied authority reached the actual launch seam")
}

#[test]
fn owned_organization_review_proves_before_preparation_and_launches_once() {
    let fixture = Fixture::new();
    let forge = fixture.forge(vec![fixture.proof.clone()]);
    let request = request(&fixture);
    let launches = Cell::new(0);
    execute(
        &queue(),
        &request,
        Some((&fixture.db, &forge)),
        || {
            assert_eq!(forge.proof_calls.load(Ordering::SeqCst), 1);
            Ok(None)
        },
        |task| {
            assert_eq!(forge.proof_calls.load(Ordering::SeqCst), 2);
            assert_eq!(task.worktree, request.snapshot.worktree_key);
            assert_eq!(task.vars.get("pr_title"), Some(fixture.pr.title.as_str()));
            assert_eq!(task.vars.get("base"), Some("main"));
            assert_eq!(task.vars.get("pr_number"), Some("7"));
            assert_eq!(task.vars.get("threads"), Some(request.feedback.as_str()));
            launches.set(launches.get() + 1);
            true
        },
    )
    .unwrap();
    assert_eq!(launches.get(), 1);
    assert_eq!(forge.reruns.load(Ordering::SeqCst), 0);
}

#[test]
fn foreign_unknown_mixed_pr_and_stale_selected_identity_never_prepare_or_launch() {
    for case in ["foreign", "unknown", "number", "head", "branch", "worktree"] {
        let fixture = Fixture::new();
        let mut request = request(&fixture);
        let mut proof = fixture.proof.clone();
        if case == "foreign" {
            proof.author.id = "U_other".into();
        }
        let mut forge = fixture.forge(vec![proof]);
        match case {
            "foreign" => forge.pr.author.as_mut().unwrap().id = "U_other".into(),
            "unknown" => forge.pr.author = None,
            "number" => forge.pr.number = 8,
            "head" => request.snapshot.head_oid = "f".repeat(40),
            "branch" => request.snapshot.branch = "other-branch".into(),
            "worktree" => request.snapshot.worktree_key.push_str("-other"),
            _ => unreachable!(),
        }
        assert!(
            execute(
                &queue(),
                &request,
                Some((&fixture.db, &forge)),
                forbid_prepare,
                forbid_launch,
            )
            .is_err(),
            "{case}"
        );
        assert_eq!(forge.reruns.load(Ordering::SeqCst), 0);
    }
}

#[test]
fn selected_url_cannot_normalize_a_foreign_authority_into_the_proof() {
    let fixture = Fixture::new();
    let forge = fixture.forge(vec![fixture.proof.clone()]);
    for url in [
        "https://foreign.invalid/organization/project/pull/7",
        "https://github.com@foreign.invalid/organization/project/pull/7",
        "https://foreign.invalid@github.com/organization/project/pull/7",
        "https://github.com/other/project/pull/7",
        "https://github.com/organization/project/pull/8",
        "https://github.com/organization/project/pull/7?next=foreign",
        "https://github.com/organization/project/pull/7#foreign",
        "https://github.com/organization/project/pull/7/../8",
    ] {
        let mut request = request(&fixture);
        request.url = url.into();
        assert!(
            execute(
                &queue(),
                &request,
                Some((&fixture.db, &forge)),
                forbid_prepare,
                forbid_launch,
            )
            .is_err(),
            "{url}"
        );
    }
}

#[test]
fn persisted_remote_malformed_and_failed_db_never_query_a_provider() {
    let fixture = Fixture::new();
    let request = request(&fixture);
    let forge = fixture.forge(vec![fixture.proof.clone()]);
    let path = fixture.dir.path().to_str().unwrap();
    fixture
        .db
        .put_worktree("fixture", path, path, "fixture", Some("local"), None)
        .unwrap();
    for location in [
        r#"{"host":"fixture.invalid","port":22,"path":"/remote"}"#,
        r#"{"control_prefix":["fixture-provider"],"path":"/remote"}"#,
        "malformed location",
    ] {
        fixture.db.set_worktree_location(path, location).unwrap();
        assert!(
            execute(
                &queue(),
                &request,
                Some((&fixture.db, &forge)),
                forbid_prepare,
                forbid_launch,
            )
            .is_err()
        );
    }
    // An error reading location is not an authoritative absent/local record.
    let conn = rusqlite::Connection::open(fixture.dir.path().join("fixture.db")).unwrap();
    conn.execute_batch("ALTER TABLE worktrees RENAME TO unavailable_worktrees")
        .unwrap();
    assert!(
        execute(
            &queue(),
            &request,
            Some((&fixture.db, &forge)),
            forbid_prepare,
            forbid_launch,
        )
        .is_err()
    );
    assert!(execute(&queue(), &request, None, forbid_prepare, forbid_launch).is_err());
    assert_eq!(forge.status_calls.load(Ordering::SeqCst), 0);
    assert_eq!(forge.proof_calls.load(Ordering::SeqCst), 0);
}

#[test]
fn late_viewer_origin_head_and_worktree_changes_hold_without_touching_owned_rows() {
    for case in ["viewer", "origin", "head", "worktree"] {
        let fixture = Fixture::new();
        let request = request(&fixture);
        let path = fixture.dir.path().to_str().unwrap();
        fixture
            .db
            .put_worktree("fixture", path, path, "fixture", Some("local"), None)
            .unwrap();
        fixture
            .db
            .enqueue_pr(path, 7, Some(path), "fixture", "main", "github")
            .unwrap();
        let dispatch_id = fixture
            .db
            .put_agent_dispatch(NewDispatch::new(
                "pr:github:organization/project#7",
                path,
                "fixture",
            ))
            .unwrap();
        fixture
            .db
            .update_dispatch_status(dispatch_id, AgentDispatchStatus::Running)
            .unwrap();
        let before = fixture.db.get_dispatch(dispatch_id).unwrap().unwrap();
        let mut after_proof = fixture.proof.clone();
        if case == "viewer" {
            after_proof.viewer.id = "U_rotated".into();
        }
        let forge = fixture.forge(vec![fixture.proof.clone(), after_proof]);
        let preparations = Cell::new(0);
        let result = execute(
            &queue(),
            &request,
            Some((&fixture.db, &forge)),
            || {
                preparations.set(preparations.get() + 1);
                assert_eq!(forge.proof_calls.load(Ordering::SeqCst), 1);
                match case {
                    "viewer" => {} // The second fresh proof returns the rotated viewer.
                    "origin" => assert!(
                        fixture
                            .loc
                            .git_command(&[
                                "remote",
                                "set-url",
                                "origin",
                                "https://github.com/other/project.git"
                            ])
                            .output()
                            .unwrap()
                            .status
                            .success()
                    ),
                    "head" => assert!(
                        fixture
                            .loc
                            .git_command(&[
                                "-c",
                                "user.name=Fixture",
                                "-c",
                                "user.email=fixture@example.invalid",
                                "-c",
                                "commit.gpgsign=false",
                                "commit",
                                "--allow-empty",
                                "-m",
                                "moved"
                            ])
                            .output()
                            .unwrap()
                            .status
                            .success()
                    ),
                    "worktree" => fixture
                        .db
                        .set_worktree_location(path, "changed-placement")
                        .unwrap(),
                    _ => unreachable!(),
                }
                Ok(None)
            },
            forbid_launch,
        );
        assert!(result.is_err(), "{case}");
        assert_eq!(preparations.get(), 1);
        assert_eq!(fixture.db.list_pr_queue().unwrap()[0].agent_attempts, 0);
        assert_eq!(
            fixture.db.get_dispatch(dispatch_id).unwrap().unwrap(),
            before
        );
        assert_eq!(forge.reruns.load(Ordering::SeqCst), 0);
    }
}

#[test]
fn explicit_broader_policy_runs_without_database_or_authorship_prerequisites() {
    let fixture = Fixture::new();
    let mut request = request(&fixture);
    request.snapshot = PrReviewSnapshot::default();
    request.url = "legacy view URL".into();
    let queue = PrQueueConfig {
        own_prs_only: false,
        ..queue()
    };
    let launches = Cell::new(0);
    execute(
        &queue,
        &request,
        None,
        || Ok(None),
        |task| {
            assert_eq!(task.vars.get("pr_title"), Some("cached title"));
            assert_eq!(task.vars.get("pr_url"), Some("legacy view URL"));
            launches.set(launches.get() + 1);
            true
        },
    )
    .unwrap();
    assert_eq!(launches.get(), 1);
}

#[test]
fn preparation_failure_never_reaches_launch_even_with_valid_authority() {
    let fixture = Fixture::new();
    let forge = fixture.forge(vec![fixture.proof.clone()]);
    let result = execute(
        &queue(),
        &request(&fixture),
        Some((&fixture.db, &forge)),
        || Err("fixture sandbox unavailable".into()),
        forbid_launch,
    );
    assert_eq!(result.as_deref(), Err("fixture sandbox unavailable"));
    assert_eq!(forge.proof_calls.load(Ordering::SeqCst), 1);
}
