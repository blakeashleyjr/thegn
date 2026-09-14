use super::*;
use std::collections::VecDeque;
use std::sync::{
    Mutex,
    atomic::{AtomicUsize, Ordering},
};
use thegn_core::forge::{
    ForgeCaps, ForgeError, PrRef, RepoRef,
    model::{PrAuthor, PrHeader},
};

pub(crate) struct Fixture {
    pub dir: tempfile::TempDir,
    pub loc: GitLoc,
    pub db: thegn_core::db::Db,
    pub pr: PrStatus,
    pub proof: PrAuthorship,
}
impl Fixture {
    pub fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let loc = GitLoc::Local(dir.path().into());
        let git = |args: &[&str]| {
            let output = loc.git_command(args).output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8(output.stdout).unwrap().trim().to_string()
        };
        git(&["init", "--quiet"]);
        git(&[
            "remote",
            "add",
            "origin",
            "https://github.com/organization/project.git",
        ]);
        git(&[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--allow-empty",
            "-m",
            "fixture",
        ]);
        let head = git(&["rev-parse", "HEAD"]);
        let author = PrAuthor {
            id: "U_me".into(),
            login: "me".into(),
            is_bot: false,
        };
        let pr = PrStatus {
            number: 7,
            title: "fixture".into(),
            url: "https://github.com/organization/project/pull/7".into(),
            state: "OPEN".into(),
            head_ref_oid: head.clone(),
            head_ref_name: "fixture".into(),
            base_ref_name: "main".into(),
            author: Some(author.clone()),
            ..Default::default()
        };
        let proof = PrAuthorship {
            provider: "github".into(),
            repository: GithubRepository::from_origin(
                "https://github.com/organization/project.git",
            )
            .unwrap(),
            repository_id: "R_project".into(),
            pr_id: "PR_7".into(),
            number: 7,
            head,
            state: "OPEN".into(),
            author: author.clone(),
            viewer: author,
        };
        let db = thegn_core::db::Db::open_at(&dir.path().join("fixture.db")).unwrap();
        Self {
            db,
            dir,
            loc,
            pr,
            proof,
        }
    }
    pub fn forge(&self, proofs: Vec<PrAuthorship>) -> FakeForge {
        FakeForge {
            pr: self.pr.clone(),
            proofs: Mutex::new(proofs.into()),
            proof_calls: AtomicUsize::new(0),
            status_calls: AtomicUsize::new(0),
            reruns: AtomicUsize::new(0),
        }
    }
}

pub(crate) struct FakeForge {
    pub pr: PrStatus,
    proofs: Mutex<VecDeque<PrAuthorship>>,
    pub proof_calls: AtomicUsize,
    pub status_calls: AtomicUsize,
    pub reruns: AtomicUsize,
}
impl thegn_core::seam::Probe for FakeForge {
    fn probe(&self) -> thegn_core::seam::ProbeReport {
        thegn_core::seam::ProbeReport::new("forge", "github", thegn_core::seam::Availability::Ready)
    }
}
impl Forge for FakeForge {
    fn id(&self) -> &'static str {
        "github"
    }
    fn caps(&self) -> ForgeCaps {
        ForgeCaps {
            pr_authorship: true,
            pr_status: true,
            ..Default::default()
        }
    }
    fn repo_ref(&self, _: &GitLoc) -> Option<RepoRef> {
        None
    }
    fn pr_status(&self, _: &GitLoc, _: PrRef) -> Result<PrStatus, ForgeError> {
        self.status_calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.pr.clone())
    }
    fn pr_list(&self, _: &GitLoc, _: usize) -> Result<Vec<PrHeader>, ForgeError> {
        Ok(vec![])
    }
    fn pr_authorship(&self, _: &GitLoc, _: u64) -> Result<PrAuthorship, ForgeError> {
        self.proof_calls.fetch_add(1, Ordering::SeqCst);
        let mut proofs = self.proofs.lock().unwrap();
        if proofs.len() > 1 {
            return Ok(proofs.pop_front().unwrap());
        }
        proofs
            .front()
            .cloned()
            .ok_or(ForgeError::Unsupported("fixture authorship"))
    }
    fn rerun_failed(&self, _: &GitLoc, _: PrRef) -> Result<u32, ForgeError> {
        self.reruns.fetch_add(1, Ordering::SeqCst);
        Ok(0)
    }
}

#[test]
fn fresh_organization_authorship_is_admitted_and_disabled_policy_needs_no_proof() {
    let fixture = Fixture::new();
    let forge = fixture.forge(vec![fixture.proof.clone()]);
    let permit = acquire(
        true,
        &fixture.db,
        &forge,
        &fixture.loc,
        "github",
        7,
        &fixture.pr,
    )
    .unwrap();
    revalidate(
        permit.as_ref(),
        &fixture.db,
        &forge,
        &fixture.loc,
        &fixture.pr,
        false,
    )
    .unwrap();
    assert_eq!(forge.proof_calls.load(Ordering::SeqCst), 2);
    let unavailable = GitLoc::Local("/does-not-exist".into());
    assert!(
        acquire(
            false,
            &fixture.db,
            &forge,
            &unavailable,
            "gitlab",
            7,
            &PrStatus::default()
        )
        .unwrap()
        .is_none()
    );
    revalidate(
        None,
        &fixture.db,
        &forge,
        &unavailable,
        &PrStatus::default(),
        false,
    )
    .unwrap();
    assert_eq!(forge.proof_calls.load(Ordering::SeqCst), 2);
}

#[test]
fn account_change_and_origin_change_after_preparation_hold() {
    let fixture = Fixture::new();
    let mut foreign = fixture.proof.clone();
    foreign.viewer.id = "U_someone_else".into();
    let forge = fixture.forge(vec![fixture.proof.clone(), foreign]);
    let permit = acquire(
        true,
        &fixture.db,
        &forge,
        &fixture.loc,
        "github",
        7,
        &fixture.pr,
    )
    .unwrap();
    assert!(
        revalidate(
            permit.as_ref(),
            &fixture.db,
            &forge,
            &fixture.loc,
            &fixture.pr,
            false
        )
        .is_err()
    );
    let forge = fixture.forge(vec![fixture.proof.clone()]);
    let permit = acquire(
        true,
        &fixture.db,
        &forge,
        &fixture.loc,
        "github",
        7,
        &fixture.pr,
    )
    .unwrap();
    assert!(
        fixture
            .loc
            .git_command(&[
                "remote",
                "set-url",
                "origin",
                "https://github.com/other/project.git"
            ])
            .status()
            .unwrap()
            .success()
    );
    assert!(
        revalidate(
            permit.as_ref(),
            &fixture.db,
            &forge,
            &fixture.loc,
            &fixture.pr,
            false
        )
        .is_err()
    );
}

#[test]
fn unknown_cached_author_local_head_and_selected_provider_are_not_authority() {
    let fixture = Fixture::new();
    let forge = fixture.forge(vec![fixture.proof.clone()]);
    let mut stale = fixture.pr.clone();
    stale.author = None;
    assert!(acquire(true, &fixture.db, &forge, &fixture.loc, "github", 7, &stale).is_err());
    assert!(
        acquire(
            true,
            &fixture.db,
            &forge,
            &fixture.loc,
            "ghe",
            7,
            &fixture.pr
        )
        .is_err()
    );
    let mut moved = fixture.pr.clone();
    moved.head_ref_oid = "f".repeat(40);
    let mut proof = fixture.proof.clone();
    proof.head = moved.head_ref_oid.clone();
    let forge = fixture.forge(vec![proof]);
    assert!(acquire(true, &fixture.db, &forge, &fixture.loc, "github", 7, &moved).is_err());
}

#[test]
fn only_verified_review_completion_can_advance_the_permitted_head() {
    let fixture = Fixture::new();
    let forge = fixture.forge(vec![fixture.proof.clone()]);
    let permit = acquire(
        true,
        &fixture.db,
        &forge,
        &fixture.loc,
        "github",
        7,
        &fixture.pr,
    )
    .unwrap();
    assert!(
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
                "verified fixture push"
            ])
            .output()
            .unwrap()
            .status
            .success()
    );
    let mut after = fixture.pr.clone();
    after.head_ref_oid = fixture.loc.git_out(&["rev-parse", "HEAD"]).unwrap();
    let mut proof = fixture.proof.clone();
    proof.head = after.head_ref_oid.clone();
    let forge = fixture.forge(vec![proof]);
    assert!(
        revalidate(
            permit.as_ref(),
            &fixture.db,
            &forge,
            &fixture.loc,
            &after,
            false
        )
        .is_err()
    );
    assert!(
        revalidate(
            permit.as_ref(),
            &fixture.db,
            &forge,
            &fixture.loc,
            &after,
            true
        )
        .is_ok()
    );
}

#[test]
fn persisted_remote_malformed_and_changed_execution_locations_hold_before_proof() {
    let fixture = Fixture::new();
    let path = fixture.dir.path().to_str().unwrap();
    let forge = fixture.forge(vec![fixture.proof.clone()]);
    let permit = acquire(
        true,
        &fixture.db,
        &forge,
        &fixture.loc,
        "github",
        7,
        &fixture.pr,
    )
    .unwrap();
    fixture
        .db
        .put_worktree("fixture", path, path, "fixture", Some("local"), None)
        .unwrap();
    for location in [
        r#"{"host":"fixture.invalid","port":22,"path":"/remote"}"#,
        r#"{"control_prefix":["fixture-provider"],"path":"/remote"}"#,
        "broken metadata",
    ] {
        fixture.db.set_worktree_location(path, location).unwrap();
        let before = forge.proof_calls.load(Ordering::SeqCst);
        assert!(
            acquire(
                true,
                &fixture.db,
                &forge,
                &fixture.loc,
                "github",
                7,
                &fixture.pr
            )
            .is_err()
        );
        assert!(
            revalidate(
                permit.as_ref(),
                &fixture.db,
                &forge,
                &fixture.loc,
                &fixture.pr,
                false
            )
            .is_err()
        );
        assert_eq!(forge.proof_calls.load(Ordering::SeqCst), before);
    }
    fixture.db.set_worktree_location(path, "local").unwrap();
    assert!(
        revalidate(
            permit.as_ref(),
            &fixture.db,
            &forge,
            &fixture.loc,
            &fixture.pr,
            false
        )
        .is_ok()
    );
    // A different checkout with identical repository/head evidence is still a
    // different selected execution location.
    let other = Fixture::new();
    assert!(
        other
            .loc
            .git_command(&["fetch", "--quiet", path, &fixture.pr.head_ref_oid])
            .output()
            .unwrap()
            .status
            .success()
    );
    assert!(
        other
            .loc
            .git_command(&["checkout", "--quiet", "--detach", "FETCH_HEAD"])
            .output()
            .unwrap()
            .status
            .success()
    );
    assert_eq!(
        other.loc.git_out(&["rev-parse", "HEAD"]),
        Some(fixture.pr.head_ref_oid.clone())
    );
    assert!(
        revalidate(
            permit.as_ref(),
            &other.db,
            &forge,
            &other.loc,
            &fixture.pr,
            false
        )
        .is_err()
    );
}
