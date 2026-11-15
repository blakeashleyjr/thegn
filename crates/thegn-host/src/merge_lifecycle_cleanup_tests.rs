//! THE-236: per-class failure injection, idempotence and concurrency for the
//! automatic landed-worktree cleanup (`remove_landed`).
//!
//! Every test drives the real `remove_landed` entry point against a private
//! repository and a file-backed state database, injects exactly one failure,
//! and asserts both the typed refusal class and that nothing was destroyed:
//! the worktree directory, the branch ref and the queue row all survive.

use super::*;
use std::path::PathBuf;

/// Every refusal class with its persisted id. Reports, notes and the sweep's
/// `kept` rows carry these ids, so they are a stored contract.
const ALL_REFUSALS: [(CleanupRefusal, &str); 8] = [
    (CleanupRefusal::Database, "database"),
    (CleanupRefusal::Identity, "identity"),
    (CleanupRefusal::ClaimBusy, "claim_busy"),
    (CleanupRefusal::MergeClaimed, "merge_claimed"),
    (CleanupRefusal::Unsafe, "unsafe"),
    (CleanupRefusal::Remote, "remote"),
    (CleanupRefusal::Resources, "resources"),
    (CleanupRefusal::Removal, "removal"),
];

/// A private repository on `main` with a linked, landed worktree on `feat`,
/// registered in a file-backed database (so concurrent callers can open their
/// own connections to it).
struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    feat: PathBuf,
    db_path: PathBuf,
    commit: String,
}

#[expect(
    clippy::disallowed_methods,
    reason = "fixture git runs against a private test repository"
)]
fn git_out(dir: &Path, args: &[&str]) -> String {
    let output = cleanup::test_git(dir).args(args).output().unwrap();
    assert!(
        output.status.success(),
        "git {} failed in {}: {}",
        args.join(" "),
        dir.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

impl Fixture {
    /// Requires a live [`TestIsolation`] on the calling thread.
    fn new() -> (Self, Db) {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().canonicalize().unwrap();
        let root = parent.join("repo");
        let feat = parent.join("feat");
        let db_path = parent.join("state.db");
        let db = Db::open_at(&db_path).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        git_out(&root, &["init", "-q", "-b", "main"]);
        git_out(&root, &["config", "user.name", "t"]);
        git_out(&root, &["config", "user.email", "t@e"]);
        git_out(&root, &["config", "commit.gpgsign", "false"]);
        std::fs::write(root.join("base.txt"), "base\n").unwrap();
        git_out(&root, &["add", "-A"]);
        git_out(&root, &["commit", "-q", "-m", "c0"]);
        git_out(
            &root,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "feat",
                feat.to_str().unwrap(),
                "main",
            ],
        );
        let root_s = root.to_string_lossy().into_owned();
        let feat_s = feat.to_string_lossy().into_owned();
        db.put_workspace(&root_s, "repo", "git").unwrap();
        db.put_worktree("feat", &root_s, &feat_s, "feat", None, None)
            .unwrap();
        let commit = git_out(&root, &["rev-parse", "HEAD"]);
        db.enqueue_merge(&feat_s, "feat", "main").unwrap();
        db.update_merge_status(&feat_s, "landed", Some(&commit), None, None)
            .unwrap();
        (
            Self {
                _dir: dir,
                root,
                feat,
                db_path,
                commit,
            },
            db,
        )
    }

    fn feat_s(&self) -> String {
        self.feat.to_string_lossy().into_owned()
    }

    fn remove(&self, db: &Db, delete_branch: bool) -> CleanupOutcome {
        remove_landed(
            db,
            &self.root,
            &self.feat_s(),
            "feat",
            "main",
            Some(&self.commit),
            delete_branch,
        )
    }

    fn branch_oid(&self) -> String {
        git_out(&self.root, &["rev-parse", "--verify", "refs/heads/feat"])
    }

    /// Queue rows plus the branch tip: the state a refusal must not touch.
    fn snapshot(&self, db: &Db) -> (Vec<thegn_core::db::MergeQueueRow>, String) {
        (db.list_merge_queue().unwrap(), self.branch_oid())
    }

    fn assert_untouched(&self, db: &Db, before: &(Vec<thegn_core::db::MergeQueueRow>, String)) {
        assert!(self.feat.is_dir(), "refused cleanup must keep the worktree");
        assert!(
            self.feat.join("base.txt").is_file(),
            "refused cleanup must keep the worktree contents"
        );
        assert_eq!(
            &self.snapshot(db),
            before,
            "refused cleanup must keep the queue row and the branch"
        );
        assert_eq!(
            removal_events(db, &self.feat_s()),
            0,
            "refused cleanup must not record a queue removal"
        );
    }
}

/// After a successful cleanup the path's audit trail is gone too: the cache
/// delete drops the landed queue row and, with no queue row left, the path's
/// `merge_queue_events` (events follow their queue row). So the explicit
/// `remove_merge_entry_as` that follows finds no row and records nothing.
fn assert_audit_trail_collected(db: &Db, worktree: &str) {
    assert!(
        db.merge_queue_events(worktree, 64).unwrap().is_empty(),
        "a removed worktree's queue events go with it"
    );
}

fn removal_events(db: &Db, worktree: &str) -> usize {
    db.merge_queue_events(worktree, 64)
        .unwrap()
        .iter()
        .filter(|event| event.operation == "remove" && event.outcome == "deleted")
        .count()
}

/// The refusal class and reason, or a panic naming the unexpected outcome.
fn refusal(outcome: &CleanupOutcome) -> (CleanupRefusal, &str) {
    match outcome {
        CleanupOutcome::Refused { class, reason } => (*class, reason.as_str()),
        other => panic!("expected a typed refusal, got {other:?}"),
    }
}

#[test]
fn cleanup_refusal_ids_are_stable_unique_and_nonempty() {
    let mut ids = std::collections::BTreeSet::new();
    let mut labels = std::collections::BTreeSet::new();
    for (class, id) in ALL_REFUSALS {
        assert_eq!(class.id(), id, "{class:?} id is a persisted contract");
        assert!(
            id.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'),
            "{id} must stay a snake_case token"
        );
        assert!(!class.label().trim().is_empty(), "{class:?} needs a label");
        assert!(ids.insert(class.id()), "duplicate id {id}");
        assert!(
            labels.insert(class.label()),
            "duplicate label for {class:?}"
        );
    }
    assert_eq!(ids.len(), 8);
    assert_eq!(labels.len(), 8);
}

#[test]
fn cleanup_outcome_resources_is_typed() {
    let _isolation = TestIsolation::new();
    let (fx, db) = Fixture::new();
    // A selected environment that resolves to no `[env.*]` table is an
    // unresolved runtime: automatic cleanup cannot prove it owns nothing.
    db.set_workspace_env(&fx.root.to_string_lossy(), "tg-undefined-cleanup-env")
        .unwrap();
    let before = fx.snapshot(&db);

    let outcome = fx.remove(&db, false);

    let (class, reason) = refusal(&outcome);
    assert_eq!(class, CleanupRefusal::Resources, "{reason}");
    assert!(
        reason.contains("requires explicit owned cleanup"),
        "{reason}"
    );
    fx.assert_untouched(&db, &before);
    assert!(db.worktree_record(&fx.feat_s()).unwrap().is_some());
}

#[test]
fn cleanup_outcome_removal_is_typed() {
    let _isolation = TestIsolation::new();
    let (fx, db) = Fixture::new();
    // A directory where the repository mutation lock file belongs makes the
    // checked removal fail after every admission check passed.
    let lock = thegn_core::util::git_common_dir(&fx.root).join("thegn-git.lock");
    std::fs::create_dir(&lock).unwrap();
    let before = fx.snapshot(&db);

    let outcome = fx.remove(&db, false);

    let (class, reason) = refusal(&outcome);
    assert_eq!(class, CleanupRefusal::Removal, "{reason}");
    assert!(reason.contains("mutation lock"), "{reason}");
    fx.assert_untouched(&db, &before);
    assert!(db.worktree_record(&fx.feat_s()).unwrap().is_some());
    std::fs::remove_dir(&lock).unwrap();
}

#[test]
fn cleanup_outcome_remote_is_typed() {
    let _isolation = TestIsolation::new();
    let (fx, db) = Fixture::new();
    db.set_worktree_location(&fx.feat_s(), "ssh:remote-box")
        .unwrap();
    let before = fx.snapshot(&db);

    let outcome = fx.remove(&db, false);

    let (class, reason) = refusal(&outcome);
    assert_eq!(class, CleanupRefusal::Remote, "{reason}");
    fx.assert_untouched(&db, &before);
    assert_eq!(
        db.worktree_record(&fx.feat_s()).unwrap().unwrap().location,
        "ssh:remote-box",
        "the remote cache row is kept as recorded"
    );
}

#[test]
fn cleanup_outcome_unsafe_is_typed() {
    let _isolation = TestIsolation::new();
    let (fx, db) = Fixture::new();
    // A locked worktree is not a verifiable unattended-removal candidate.
    git_out(&fx.root, &["worktree", "lock", fx.feat.to_str().unwrap()]);
    let before = fx.snapshot(&db);

    let outcome = fx.remove(&db, false);

    let (class, reason) = refusal(&outcome);
    assert_eq!(class, CleanupRefusal::Unsafe, "{reason}");
    assert!(reason.contains("locked"), "{reason}");
    fx.assert_untouched(&db, &before);
    assert!(db.worktree_record(&fx.feat_s()).unwrap().is_some());
}

#[test]
fn cleanup_outcome_database_is_typed() {
    let _isolation = TestIsolation::new();
    let (fx, db) = Fixture::new();
    // An undecodable cache row: the queue still reads, the cache identity
    // cannot, and an unreadable identity is never treated as absent.
    rusqlite::Connection::open(&fx.db_path)
        .unwrap()
        .execute(
            "UPDATE worktrees SET position='malformed' WHERE worktree=?1",
            [fx.feat_s()],
        )
        .unwrap();
    assert!(db.worktree_record(&fx.feat_s()).is_err());
    let before = fx.snapshot(&db);

    let outcome = fx.remove(&db, false);

    let (class, reason) = refusal(&outcome);
    assert_eq!(class, CleanupRefusal::Database, "{reason}");
    assert!(
        reason.contains("worktree cache identity unavailable"),
        "{reason}"
    );
    fx.assert_untouched(&db, &before);
}

#[test]
fn second_remove_after_success_is_typed_and_removes_queue_row_once() {
    let _isolation = TestIsolation::new();
    let (fx, db) = Fixture::new();
    let branch = fx.branch_oid();

    let first = fx.remove(&db, false);
    assert!(
        matches!(
            first,
            CleanupOutcome::Removed {
                queue_removed: true,
                branch_deleted: false,
                ..
            }
        ),
        "{first:?}"
    );
    assert!(!fx.feat.exists());
    assert!(
        db.list_merge_queue()
            .unwrap()
            .iter()
            .all(|row| row.worktree != fx.feat_s())
    );
    assert_audit_trail_collected(&db, &fx.feat_s());

    // A replayed cleanup (a retried sweep, a crashed caller re-running) finds
    // no landed row and refuses without touching anything.
    let second = fx.remove(&db, false);
    let (class, reason) = refusal(&second);
    assert_eq!(class, CleanupRefusal::Identity, "{reason}");
    assert!(!fx.feat.exists());
    assert_audit_trail_collected(&db, &fx.feat_s());
    assert_eq!(
        fx.branch_oid(),
        branch,
        "automatic cleanup keeps the branch"
    );
}

#[test]
fn refused_cleanup_is_idempotent_and_retryable() {
    let _isolation = TestIsolation::new();
    let (fx, db) = Fixture::new();
    let lock = thegn_core::util::git_common_dir(&fx.root).join("thegn-git.lock");
    std::fs::create_dir(&lock).unwrap();
    let before = fx.snapshot(&db);

    for attempt in 0..2 {
        let outcome = fx.remove(&db, false);
        let (class, reason) = refusal(&outcome);
        assert_eq!(
            class,
            CleanupRefusal::Removal,
            "attempt {attempt}: {reason}"
        );
        fx.assert_untouched(&db, &before);
        assert!(db.worktree_record(&fx.feat_s()).unwrap().is_some());
    }

    // Clearing the fault makes the very same retry succeed.
    std::fs::remove_dir(&lock).unwrap();
    let retried = fx.remove(&db, false);
    assert!(
        matches!(
            retried,
            CleanupOutcome::Removed {
                queue_removed: true,
                ..
            }
        ),
        "{retried:?}"
    );
    assert!(!fx.feat.exists());
    assert_audit_trail_collected(&db, &fx.feat_s());
}

#[test]
fn concurrent_remove_landed_removes_exactly_once() {
    let _isolation = TestIsolation::new();
    let (fx, db) = Fixture::new();
    let branch = fx.branch_oid();
    let barrier = std::sync::Barrier::new(2);
    let connections = [
        Db::open_at(&fx.db_path).unwrap(),
        Db::open_at(&fx.db_path).unwrap(),
    ];
    let fixture = &fx;
    let barrier = &barrier;

    let outcomes: Vec<CleanupOutcome> = std::thread::scope(|scope| {
        let handles: Vec<_> = connections
            .into_iter()
            .map(|connection| {
                scope.spawn(move || {
                    barrier.wait();
                    fixture.remove(&connection, false)
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect()
    });

    let removed = outcomes
        .iter()
        .filter(|outcome| matches!(outcome, CleanupOutcome::Removed { .. }))
        .count();
    assert_eq!(removed, 1, "exactly one caller removes: {outcomes:?}");
    let loser = outcomes
        .iter()
        .find(|outcome| !matches!(outcome, CleanupOutcome::Removed { .. }))
        .unwrap();
    // The loser either met the held destroy claim, found the row already
    // collected, or found the path already gone. All are typed and inert.
    let (class, reason) = refusal(loser);
    assert!(
        matches!(
            class,
            CleanupRefusal::ClaimBusy | CleanupRefusal::Identity | CleanupRefusal::Unsafe
        ),
        "{class:?}: {reason}"
    );
    assert!(!fx.feat.exists());
    assert!(
        db.list_merge_queue()
            .unwrap()
            .iter()
            .all(|row| row.worktree != fx.feat_s())
    );
    assert_audit_trail_collected(&db, &fx.feat_s());
    assert_eq!(fx.branch_oid(), branch);
}
