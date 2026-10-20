use super::*;
use crate::store::{MergeCandidate, MergeClaimRequest, MergeFinalOutcome, MergeFinalStatus};
use crate::store::{WorkspaceStore, WorktreeAuxStore};

const WORKTREE: &str = "/private/repo/feature";
const REPO: &str = "/private/repo";

fn real_candidate_oids() -> (tempfile::TempDir, String, String) {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    run_git(&repo, &["init", "-q", "-b", "main"]);
    run_git(&repo, &["config", "user.name", "Candidate Test"]);
    run_git(
        &repo,
        &["config", "user.email", "candidate@example.invalid"],
    );
    std::fs::write(repo.join("candidate.txt"), "one\n").unwrap();
    run_git(&repo, &["add", "-A"]);
    run_git(
        &repo,
        &["-c", "commit.gpgsign=false", "commit", "-q", "-m", "one"],
    );
    let oid_a = run_git(&repo, &["rev-parse", "HEAD"]);
    std::fs::write(repo.join("candidate.txt"), "two\n").unwrap();
    run_git(&repo, &["add", "-A"]);
    run_git(
        &repo,
        &["-c", "commit.gpgsign=false", "commit", "-q", "-m", "two"],
    );
    let oid_b = run_git(&repo, &["rev-parse", "HEAD"]);
    (temp, oid_a, oid_b)
}

fn run_git(repo: &std::path::Path, args: &[&str]) -> String {
    let output = crate::util::git_cmd(repo)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn candidate(oid: &str) -> MergeCandidate {
    MergeCandidate {
        branch_ref: "refs/heads/feature".into(),
        candidate_oid: oid.into(),
    }
}

fn bound(db: &Db, oid: &str) {
    db.enqueue_merge_candidate(WORKTREE, "feature", "main", &candidate(oid))
        .unwrap();
}

#[test]
fn bound_enqueue_records_ref_and_oid() {
    let (_temp, oid_a, _) = real_candidate_oids();
    let db = Db::open_memory().unwrap();
    bound(&db, &oid_a);
    assert_eq!(
        db.merge_candidate(WORKTREE).unwrap(),
        Some(candidate(&oid_a))
    );
}

#[test]
fn unbound_enqueue_clears_a_bound_pair() {
    let (_temp, oid_a, _) = real_candidate_oids();
    let db = Db::open_memory().unwrap();
    bound(&db, &oid_a);
    db.enqueue_merge(WORKTREE, "feature", "main").unwrap();
    assert_eq!(db.merge_candidate(WORKTREE).unwrap(), None);
}

#[test]
fn reenqueue_adopts_the_new_oid() {
    let (_temp, oid_a, oid_b) = real_candidate_oids();
    let db = Db::open_memory().unwrap();
    bound(&db, &oid_a);
    bound(&db, &oid_b);
    assert_eq!(
        db.merge_candidate(WORKTREE).unwrap(),
        Some(candidate(&oid_b))
    );
}

#[test]
fn malformed_candidate_is_refused_and_writes_nothing() {
    let (_temp, oid_a, _) = real_candidate_oids();
    let db = Db::open_memory().unwrap();
    let mut uppercase = oid_a.clone();
    let alpha = oid_a
        .bytes()
        .position(|byte| (b'a'..=b'f').contains(&byte))
        .unwrap();
    uppercase.replace_range(alpha..alpha + 1, &oid_a[alpha..alpha + 1].to_uppercase());
    let mut non_hex = oid_a.clone();
    non_hex.replace_range(..1, "g");
    for invalid in [
        MergeCandidate {
            branch_ref: "feature".into(),
            candidate_oid: oid_a.clone(),
        },
        MergeCandidate {
            branch_ref: "refs/heads/feature".into(),
            candidate_oid: oid_a[..4].into(),
        },
        MergeCandidate {
            branch_ref: "refs/heads/feature".into(),
            candidate_oid: uppercase,
        },
        MergeCandidate {
            branch_ref: "refs/heads/feature".into(),
            candidate_oid: non_hex,
        },
    ] {
        assert!(
            db.enqueue_merge_candidate(WORKTREE, "feature", "main", &invalid)
                .is_err()
        );
        assert!(db.list_merge_queue().unwrap().is_empty());
        assert_eq!(db.merge_candidate(WORKTREE).unwrap(), None);
    }
}

#[test]
fn outcome_retry_and_claim_preserve_the_pair() {
    let (_temp, oid_a, _) = real_candidate_oids();
    let db = Db::open_memory().unwrap();
    db.put_worktree("feature", REPO, WORKTREE, "feature", None, None)
        .unwrap();
    bound(&db, &oid_a);
    let observed = db.observe_merge_outcome(WORKTREE).unwrap();
    db.persist_merge_outcome(
        &observed,
        &MergeFinalOutcome {
            worktree: WORKTREE,
            branch: "feature",
            repo_root: REPO,
            target_branch: "main",
            location: "",
            status: MergeFinalStatus::Deferred,
            result_oid: None,
            conflict_paths: None,
            error_detail: Some("retry me"),
        },
    )
    .unwrap();
    assert!(db.retry_merge_entry(WORKTREE).unwrap());
    assert_eq!(
        db.merge_candidate(WORKTREE).unwrap(),
        Some(candidate(&oid_a))
    );
    let claim = db
        .admit_merge_claim(&MergeClaimRequest {
            worktree: WORKTREE.into(),
            branch: "feature".into(),
            repo_root: REPO.into(),
            target_branch: "main".into(),
            location: String::new(),
            expected_status: "queued".into(),
            lease_until_ms: crate::util::now_ms() + 60_000,
            owner_hint: Some("candidate-test".into()),
        })
        .unwrap()
        .unwrap();
    db.release_merge_claim(&claim).unwrap();
    assert_eq!(
        db.merge_candidate(WORKTREE).unwrap(),
        Some(candidate(&oid_a))
    );
}

#[test]
fn remote_matched_enqueue_is_unbound() {
    let (_temp, oid_a, _) = real_candidate_oids();
    let db = Db::open_memory().unwrap();
    db.put_worktree(
        "feature",
        REPO,
        WORKTREE,
        "feature",
        Some("remote-location"),
        None,
    )
    .unwrap();
    bound(&db, &oid_a);
    assert!(
        db.enqueue_merge_if_worktree_matches(WORKTREE, "feature", REPO, "remote-location", "main")
            .unwrap()
    );
    assert_eq!(db.merge_candidate(WORKTREE).unwrap(), None);
}
