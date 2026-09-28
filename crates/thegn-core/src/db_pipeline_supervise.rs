//! The supervisor's two ledgers — sibling `impl Db` block so the pinned
//! `db.rs` carries only the schema DDL, exactly as `db_dispatch.rs` does for
//! the roster.
//!
//! **What is stored, and what is not.** These tables record *facts*: a
//! validation command ran against this commit and established this class; this
//! person approved this commit. Nothing here stores a decision about what to do
//! with those facts — that is [`crate::pipeline_supervise::plan`], and keeping
//! it out of the database is what lets the policy change without a migration
//! and be unit-tested without one.
//!
//! **Everything is keyed on a commit.** A result recorded against the lane, or
//! against the roster row, would go on applying after the lane moved — which is
//! precisely how unreviewed code gets landed under an old approval. The schema's
//! `UNIQUE` constraints and every read here carry the commit through.

use crate::db::Db;
use crate::pipeline_approval::Approval;
use crate::pipeline_supervise::ValidationRecord;
use crate::pipeline_validate::ValidationClass;
use anyhow::Result;
use rusqlite::OptionalExtension as _;

/// The outcome of recording one validation run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ValidationWrite {
    /// How many times this task has now run against this commit. The planner
    /// bounds retries on it, so it must count attempts and not rows.
    pub attempts: u32,
    /// Whether this replaced an earlier result for the same (row, task,
    /// commit) rather than inserting a new one.
    pub replaced: bool,
}

/// One validation run, ready to be recorded.
///
/// A struct rather than a parameter list because the four identifying fields
/// (row, task, commit, class) are easy to transpose at a call site and every
/// transposition is silent: recording a class against the wrong commit is
/// exactly the failure the commit-keying exists to prevent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ValidationOutcome<'a> {
    /// The roster row that was validated.
    pub dispatch_id: i64,
    /// The `[[tasks]]` entry that ran.
    pub task: &'a str,
    /// The lane tip it ran against.
    pub commit_sha: &'a str,
    /// What it established.
    pub class: ValidationClass,
    /// The command's exit status, when it had one.
    pub exit_code: Option<i32>,
    /// The capped output digest (see [`crate::pipeline_validate::digest`]).
    pub digest: &'a str,
}

impl Db {
    /// Record what a validation run established.
    ///
    /// Upserts on (row, task, commit): re-running the same task against the
    /// same tree **updates** the result and increments `attempts` rather than
    /// appending a second row. That is deliberate — the planner asks "what is
    /// the current result for this tip", and a history of rows would make that
    /// question ambiguous exactly when it matters (a retried environment
    /// failure that later passes must read as passed, not as both).
    pub fn record_validation(
        &self,
        v: &ValidationOutcome<'_>,
        now_ms: i64,
    ) -> Result<ValidationWrite> {
        let ValidationOutcome {
            dispatch_id,
            task,
            commit_sha,
            class,
            exit_code,
            digest,
        } = *v;
        let conn = self.conn();
        let tx = conn.unchecked_transaction()?;
        let existing: Option<u32> = tx
            .query_row(
                "SELECT attempts FROM pipeline_validations \
                 WHERE dispatch_id=?1 AND task=?2 AND commit_sha=?3",
                rusqlite::params![dispatch_id, task, commit_sha],
                |row| row.get(0),
            )
            .optional()?;
        let attempts = existing.unwrap_or(0).saturating_add(1);
        tx.execute(
            "INSERT INTO pipeline_validations \
               (dispatch_id, task, commit_sha, class, exit_code, digest, attempts, ran_at_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) \
             ON CONFLICT (dispatch_id, task, commit_sha) DO UPDATE SET \
               class=excluded.class, exit_code=excluded.exit_code, digest=excluded.digest, \
               attempts=excluded.attempts, ran_at_ms=excluded.ran_at_ms",
            rusqlite::params![
                dispatch_id,
                task,
                commit_sha,
                class.as_str(),
                exit_code,
                digest,
                attempts,
                now_ms,
            ],
        )?;
        tx.commit()?;
        Ok(ValidationWrite {
            attempts,
            replaced: existing.is_some(),
        })
    }

    /// Every validation recorded for one roster row, at any commit.
    ///
    /// Deliberately not filtered to the current tip here: the planner does that
    /// filtering itself (and is tested on it), and `supervise validations` wants
    /// the history so a person can see that a lane was green before the last
    /// commit.
    pub fn validations_for_dispatch(&self, dispatch_id: i64) -> Result<Vec<ValidationRecord>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT task, class, commit_sha, attempts FROM pipeline_validations \
             WHERE dispatch_id=?1 ORDER BY ran_at_ms DESC, id DESC",
        )?;
        let rows = stmt
            .query_map([dispatch_id], |row| {
                Ok(ValidationRecord {
                    task: row.get::<_, String>(0)?,
                    class: ValidationClass::parse(&row.get::<_, String>(1)?),
                    commit_sha: row.get::<_, String>(2)?,
                    attempts: row.get::<_, u32>(3)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// One validation's stored digest, for the detail view. Kept off
    /// [`ValidationRecord`] so the planner never carries kilobytes of build
    /// output it has no use for.
    pub fn validation_digest(
        &self,
        dispatch_id: i64,
        task: &str,
        commit_sha: &str,
    ) -> Result<Option<String>> {
        let conn = self.conn();
        Ok(conn
            .query_row(
                "SELECT digest FROM pipeline_validations \
                 WHERE dispatch_id=?1 AND task=?2 AND commit_sha=?3",
                rusqlite::params![dispatch_id, task, commit_sha],
                |row| row.get::<_, String>(0),
            )
            .optional()?)
    }

    /// Record an approval.
    ///
    /// Re-approving the same commit updates the existing record and **clears
    /// any revocation** — an explicit re-approval after a revoke is a new
    /// decision, and leaving the old `revoked_at_ms` set would silently ignore
    /// it.
    pub fn grant_approval(&self, a: &Approval) -> Result<()> {
        let conn = self.conn();
        conn.execute(
            "INSERT INTO pipeline_approvals \
               (issue_id, stage, commit_sha, approver, note, granted_at_ms, expires_at_ms, revoked_at_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL) \
             ON CONFLICT (issue_id, stage, commit_sha) DO UPDATE SET \
               approver=excluded.approver, note=excluded.note, \
               granted_at_ms=excluded.granted_at_ms, expires_at_ms=excluded.expires_at_ms, \
               revoked_at_ms=NULL",
            rusqlite::params![
                a.issue_id,
                a.stage,
                a.commit_sha,
                a.approver,
                a.note,
                a.granted_at_ms,
                a.expires_at_ms,
            ],
        )?;
        Ok(())
    }

    /// Withdraw every live approval for a lane and stage. Returns how many were
    /// withdrawn (`0` = there was nothing to withdraw, which is not an error —
    /// revoking is a safety action and must always be allowed to succeed).
    ///
    /// The record is kept rather than deleted: "approved and then withdrawn" is
    /// a different fact from "never approved", and the audit trail needs both.
    pub fn revoke_approvals(&self, issue_id: &str, stage: &str, now_ms: i64) -> Result<usize> {
        let conn = self.conn();
        let n = conn.execute(
            "UPDATE pipeline_approvals SET revoked_at_ms=?3 \
             WHERE issue_id=?1 AND stage=?2 AND revoked_at_ms IS NULL",
            rusqlite::params![issue_id, stage, now_ms],
        )?;
        Ok(n)
    }

    /// The most recent approval recorded for a lane and stage, revoked or not.
    ///
    /// Deliberately returns the latest rather than only a matching one: the
    /// caller compares its commit against the lane's tip, and a *mismatch* is
    /// the useful answer — it lets the refusal say "you approved `abc1234`, the
    /// lane is at `def5678`" instead of "no approval", which would send the
    /// reviewer looking for something they already did.
    pub fn latest_approval(&self, issue_id: &str, stage: &str) -> Result<Option<Approval>> {
        let conn = self.conn();
        Ok(conn
            .query_row(
                "SELECT issue_id, stage, commit_sha, approver, note, granted_at_ms, \
                        expires_at_ms, revoked_at_ms \
                 FROM pipeline_approvals WHERE issue_id=?1 AND stage=?2 \
                 ORDER BY granted_at_ms DESC, id DESC LIMIT 1",
                rusqlite::params![issue_id, stage],
                approval_from_row,
            )
            .optional()?)
    }

    /// Every approval on record, newest first — the audit surface.
    pub fn list_approvals(&self, limit: usize) -> Result<Vec<Approval>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT issue_id, stage, commit_sha, approver, note, granted_at_ms, \
                    expires_at_ms, revoked_at_ms \
             FROM pipeline_approvals ORDER BY granted_at_ms DESC, id DESC LIMIT ?1",
        )?;
        let rows = stmt
            .query_map([limit as i64], approval_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }
}

fn approval_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Approval> {
    Ok(Approval {
        issue_id: row.get(0)?,
        stage: row.get(1)?,
        commit_sha: row.get(2)?,
        approver: row.get(3)?,
        note: row.get(4)?,
        granted_at_ms: row.get(5)?,
        expires_at_ms: row.get(6)?,
        revoked_at_ms: row.get(7)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline_approval::{ApprovalState, state_for};

    const HEAD: &str = "4babb0901f2c3d4e5f60718293a4b5c6d7e8f901";
    const OLD: &str = "1111111111111111111111111111111111111111";
    const NOW: i64 = 1_700_000_000_000;

    fn db() -> Db {
        Db::open_memory().expect("in-memory db")
    }

    /// A recordable run, so the tests read as facts rather than positional
    /// argument lists.
    fn run(
        dispatch_id: i64,
        task: &'static str,
        commit: &'static str,
        class: ValidationClass,
        exit_code: Option<i32>,
        digest: &'static str,
    ) -> ValidationOutcome<'static> {
        ValidationOutcome {
            dispatch_id,
            task,
            commit_sha: commit,
            class,
            exit_code,
            digest,
        }
    }

    fn approval(commit: &str) -> Approval {
        Approval {
            issue_id: "linear:THE-407".into(),
            stage: "code".into(),
            commit_sha: commit.into(),
            approver: "blake".into(),
            note: Some("scoped to the registry split".into()),
            granted_at_ms: NOW,
            expires_at_ms: None,
            revoked_at_ms: None,
        }
    }

    // --- validations ---------------------------------------------------------

    #[test]
    fn a_recorded_validation_reads_back_with_its_class_and_commit() {
        let db = db();
        let w = db
            .record_validation(
                &run(
                    7,
                    "nextest",
                    HEAD,
                    ValidationClass::TestFailure,
                    Some(100),
                    "FAIL config::a",
                ),
                NOW,
            )
            .unwrap();
        assert_eq!(w.attempts, 1);
        assert!(!w.replaced);

        let got = db.validations_for_dispatch(7).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].task, "nextest");
        assert_eq!(got[0].class, ValidationClass::TestFailure);
        assert_eq!(got[0].commit_sha, HEAD);
        assert_eq!(
            db.validation_digest(7, "nextest", HEAD).unwrap().as_deref(),
            Some("FAIL config::a")
        );
    }

    #[test]
    fn re_running_a_task_for_the_same_commit_replaces_and_counts_the_attempt() {
        // A retried environment failure that later passes must read as PASSED,
        // not as both — which is why this upserts rather than appends.
        let db = db();
        db.record_validation(
            &run(
                7,
                "nextest",
                HEAD,
                ValidationClass::EnvironmentError,
                None,
                "d",
            ),
            NOW,
        )
        .unwrap();
        let w = db
            .record_validation(
                &run(7, "nextest", HEAD, ValidationClass::Green, Some(0), ""),
                NOW + 1,
            )
            .unwrap();
        assert_eq!(w.attempts, 2);
        assert!(w.replaced);

        let got = db.validations_for_dispatch(7).unwrap();
        assert_eq!(got.len(), 1, "a second row was appended: {got:?}");
        assert_eq!(got[0].class, ValidationClass::Green);
        assert_eq!(got[0].attempts, 2);
    }

    #[test]
    fn a_result_for_a_different_commit_is_a_separate_record() {
        // The keying rule: results are statements about a tree, so a new commit
        // starts a new one rather than overwriting the old.
        let db = db();
        db.record_validation(
            &run(7, "nextest", OLD, ValidationClass::Green, Some(0), ""),
            NOW,
        )
        .unwrap();
        let w = db
            .record_validation(
                &run(7, "nextest", HEAD, ValidationClass::Green, Some(0), ""),
                NOW + 1,
            )
            .unwrap();
        assert_eq!(w.attempts, 1, "attempts leaked across commits");
        assert!(!w.replaced);
        assert_eq!(db.validations_for_dispatch(7).unwrap().len(), 2);
    }

    #[test]
    fn different_tasks_on_one_row_are_kept_apart() {
        let db = db();
        db.record_validation(
            &run(
                7,
                "clippy",
                HEAD,
                ValidationClass::LintFinding,
                Some(1),
                "w",
            ),
            NOW,
        )
        .unwrap();
        db.record_validation(
            &run(7, "nextest", HEAD, ValidationClass::Green, Some(0), ""),
            NOW,
        )
        .unwrap();
        let got = db.validations_for_dispatch(7).unwrap();
        assert_eq!(got.len(), 2);
        assert!(got.iter().any(|v| v.task == "clippy"));
        assert!(got.iter().any(|v| v.task == "nextest"));
    }

    #[test]
    fn an_unknown_row_has_no_validations_rather_than_erroring() {
        assert!(db().validations_for_dispatch(999).unwrap().is_empty());
        assert!(db().validation_digest(999, "t", HEAD).unwrap().is_none());
    }

    #[test]
    fn a_class_written_by_a_future_build_still_reads_back() {
        // Totality: the roster stays listable even when a newer build recorded
        // a class this one does not know.
        let db = db();
        db.conn()
            .execute(
                "INSERT INTO pipeline_validations \
                   (dispatch_id, task, commit_sha, class, digest, attempts, ran_at_ms) \
                 VALUES (7, 'nextest', ?1, 'flaky-quarantined', '', 1, ?2)",
                rusqlite::params![HEAD, NOW],
            )
            .unwrap();
        let got = db.validations_for_dispatch(7).unwrap();
        assert_eq!(got[0].class, ValidationClass::Inconclusive);
    }

    // --- approvals -----------------------------------------------------------

    #[test]
    fn a_granted_approval_reads_back_whole() {
        let db = db();
        let a = approval(HEAD);
        db.grant_approval(&a).unwrap();
        let got = db
            .latest_approval("linear:THE-407", "code")
            .unwrap()
            .unwrap();
        assert_eq!(got, a);
        assert!(state_for(Some(&got), HEAD, NOW, 0).is_live());
    }

    #[test]
    fn the_latest_approval_is_returned_even_when_it_covers_an_older_commit() {
        // So the refusal can say WHAT was approved. Returning `None` here would
        // report "no approval" to someone who had in fact approved something,
        // and send them looking for work they already did.
        let db = db();
        db.grant_approval(&approval(OLD)).unwrap();
        let got = db
            .latest_approval("linear:THE-407", "code")
            .unwrap()
            .unwrap();
        assert_eq!(got.commit_sha, OLD);
        match state_for(Some(&got), HEAD, NOW, 0) {
            ApprovalState::Superseded { approved, head } => {
                assert_eq!(approved, OLD);
                assert_eq!(head, HEAD);
            }
            other => panic!("expected Superseded, got {other:?}"),
        }
    }

    #[test]
    fn a_newer_approval_wins_over_an_older_one() {
        let db = db();
        db.grant_approval(&approval(OLD)).unwrap();
        let mut newer = approval(HEAD);
        newer.granted_at_ms = NOW + 1_000;
        db.grant_approval(&newer).unwrap();
        let got = db
            .latest_approval("linear:THE-407", "code")
            .unwrap()
            .unwrap();
        assert_eq!(got.commit_sha, HEAD);
    }

    #[test]
    fn revoking_withdraws_every_live_approval_for_the_lane_and_stage() {
        let db = db();
        db.grant_approval(&approval(OLD)).unwrap();
        db.grant_approval(&approval(HEAD)).unwrap();
        let n = db
            .revoke_approvals("linear:THE-407", "code", NOW + 5)
            .unwrap();
        assert_eq!(n, 2);
        let got = db
            .latest_approval("linear:THE-407", "code")
            .unwrap()
            .unwrap();
        assert_eq!(got.revoked_at_ms, Some(NOW + 5));
        assert!(!state_for(Some(&got), HEAD, NOW + 6, 0).is_live());
    }

    #[test]
    fn revoking_nothing_succeeds_because_revoking_must_always_be_possible() {
        assert_eq!(
            db().revoke_approvals("linear:THE-1", "code", NOW).unwrap(),
            0
        );
    }

    #[test]
    fn re_approving_after_a_revoke_clears_the_revocation() {
        // Otherwise the new decision is silently ignored and the lane stays
        // stuck with no visible reason.
        let db = db();
        db.grant_approval(&approval(HEAD)).unwrap();
        db.revoke_approvals("linear:THE-407", "code", NOW + 1)
            .unwrap();
        let mut again = approval(HEAD);
        again.granted_at_ms = NOW + 2;
        db.grant_approval(&again).unwrap();
        let got = db
            .latest_approval("linear:THE-407", "code")
            .unwrap()
            .unwrap();
        assert_eq!(got.revoked_at_ms, None);
        assert!(state_for(Some(&got), HEAD, NOW + 3, 0).is_live());
    }

    #[test]
    fn approvals_are_scoped_to_their_lane_and_stage() {
        let db = db();
        db.grant_approval(&approval(HEAD)).unwrap();
        assert!(
            db.latest_approval("linear:THE-407", "review")
                .unwrap()
                .is_none()
        );
        assert!(
            db.latest_approval("linear:THE-999", "code")
                .unwrap()
                .is_none()
        );
        // And revoking one stage leaves the other alone.
        let mut other = approval(HEAD);
        other.stage = "review".into();
        db.grant_approval(&other).unwrap();
        db.revoke_approvals("linear:THE-407", "code", NOW + 1)
            .unwrap();
        let review = db
            .latest_approval("linear:THE-407", "review")
            .unwrap()
            .unwrap();
        assert_eq!(review.revoked_at_ms, None);
    }

    #[test]
    fn the_audit_listing_is_newest_first_and_bounded() {
        let db = db();
        for i in 0..5 {
            let mut a = approval(&format!("{i}babb0901f2c3d4e5f60718293a4b5c6d7e8f901"));
            a.granted_at_ms = NOW + i64::from(i);
            db.grant_approval(&a).unwrap();
        }
        let got = db.list_approvals(3).unwrap();
        assert_eq!(got.len(), 3);
        assert!(got[0].granted_at_ms > got[1].granted_at_ms);
    }

    #[test]
    fn the_migration_is_idempotent() {
        // Shared multi-branch state databases re-run every migration on open.
        let db = db();
        crate::db_migrate::migrate_v70(db.conn()).unwrap();
        crate::db_migrate::migrate_v70(db.conn()).unwrap();
        db.grant_approval(&approval(HEAD)).unwrap();
        assert!(
            db.latest_approval("linear:THE-407", "code")
                .unwrap()
                .is_some()
        );
    }
}
