//! SQLite persistence for approval effects (schema v113, THE-2200/2202):
//! `automation_proposals` (a `propose` start waiting for the owner) and
//! `approval_push_nonces` (single-use phone answers, stored hashed).
//!
//! Both tables are written only by the daemon.

use anyhow::Result;
use rusqlite::{OptionalExtension, params};

use crate::db::Db;

/// Settled (not pending) proposals are kept this long after creation.
pub const PROPOSAL_RETENTION_MS: i64 = 30 * 24 * 60 * 60 * 1000;
/// Push nonces are kept this long after creation, used or not.
pub const NONCE_RETENTION_MS: i64 = 2 * 24 * 60 * 60 * 1000;

/// One stored proposal. `(approval_issue, approval_stage, approval_sha)` is
/// the approval triple it waits on (unique).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProposalRow {
    pub id: i64,
    pub approval_issue: String,
    pub approval_stage: String,
    pub approval_sha: String,
    pub rule_id: String,
    pub repo_root: String,
    pub issue_id: String,
    pub provider: String,
    pub account: String,
    pub stage: String,
    pub base_sha: String,
    pub created_at_ms: i64,
    pub expires_at_ms: i64,
    pub state: String,
}

const PROPOSAL_COLS: &str = "id, approval_issue, approval_stage, approval_sha, rule_id, repo_root, \
     issue_id, provider, account, stage, base_sha, created_at_ms, expires_at_ms, state";

fn map_proposal(row: &rusqlite::Row<'_>) -> rusqlite::Result<ProposalRow> {
    Ok(ProposalRow {
        id: row.get(0)?,
        approval_issue: row.get(1)?,
        approval_stage: row.get(2)?,
        approval_sha: row.get(3)?,
        rule_id: row.get(4)?,
        repo_root: row.get(5)?,
        issue_id: row.get(6)?,
        provider: row.get(7)?,
        account: row.get(8)?,
        stage: row.get(9)?,
        base_sha: row.get(10)?,
        created_at_ms: row.get(11)?,
        expires_at_ms: row.get(12)?,
        state: row.get(13)?,
    })
}

impl Db {
    /// Insert a proposal (`p.id` is ignored). `Ok(false)` when a proposal
    /// with the same approval triple already exists (INSERT OR IGNORE).
    pub fn put_proposal(&self, p: &ProposalRow) -> Result<bool> {
        let inserted = self.conn().execute(
            "INSERT OR IGNORE INTO automation_proposals \
             (approval_issue, approval_stage, approval_sha, rule_id, repo_root, issue_id, \
              provider, account, stage, base_sha, created_at_ms, expires_at_ms, state) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
            params![
                p.approval_issue,
                p.approval_stage,
                p.approval_sha,
                p.rule_id,
                p.repo_root,
                p.issue_id,
                p.provider,
                p.account,
                p.stage,
                p.base_sha,
                p.created_at_ms,
                p.expires_at_ms,
                p.state,
            ],
        )?;
        Ok(inserted > 0)
    }

    /// The proposal waiting on approval triple `(issue, stage, sha)`.
    pub fn proposal_for(&self, issue: &str, stage: &str, sha: &str) -> Result<Option<ProposalRow>> {
        Ok(self
            .conn()
            .query_row(
                &format!(
                    "SELECT {PROPOSAL_COLS} FROM automation_proposals \
                     WHERE approval_issue=?1 AND approval_stage=?2 AND approval_sha=?3"
                ),
                params![issue, stage, sha],
                map_proposal,
            )
            .optional()?)
    }

    /// Compare-and-set a proposal's state: `Ok(true)` only when the row was
    /// in `from` and is now `to`.
    pub fn set_proposal_state(&self, id: i64, from: &str, to: &str) -> Result<bool> {
        let changed = self.conn().execute(
            "UPDATE automation_proposals SET state=?3 WHERE id=?1 AND state=?2",
            params![id, from, to],
        )?;
        Ok(changed > 0)
    }

    /// Store a push nonce by its hash, bound to one approval triple.
    pub fn put_push_nonce(
        &self,
        nonce_hash: &str,
        issue: &str,
        stage: &str,
        sha: &str,
        now_ms: i64,
        expires_at_ms: i64,
    ) -> Result<()> {
        self.conn().execute(
            "INSERT INTO approval_push_nonces \
             (nonce_hash, issue_id, stage, commit_sha, created_at_ms, expires_at_ms) \
             VALUES (?1,?2,?3,?4,?5,?6)",
            params![nonce_hash, issue, stage, sha, now_ms, expires_at_ms],
        )?;
        Ok(())
    }

    /// Consume a nonce exactly once: one UPDATE claims it only while unused
    /// and unexpired, then the bound triple is read back. `None` for an
    /// unknown, used or expired nonce.
    pub fn consume_push_nonce(
        &self,
        nonce_hash: &str,
        now_ms: i64,
    ) -> Result<Option<(String, String, String)>> {
        let claimed = self.conn().execute(
            "UPDATE approval_push_nonces SET used_at_ms=?2 \
             WHERE nonce_hash=?1 AND used_at_ms IS NULL AND expires_at_ms > ?2",
            params![nonce_hash, now_ms],
        )?;
        if claimed == 0 {
            return Ok(None);
        }
        Ok(self
            .conn()
            .query_row(
                "SELECT issue_id, stage, commit_sha FROM approval_push_nonces WHERE nonce_hash=?1",
                params![nonce_hash],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?)
    }

    /// Burn every unused nonce bound to a triple once that request is answered
    /// elsewhere, so a stale phone push cannot re-resolve it later.
    pub fn void_push_nonces(
        &self,
        issue: &str,
        stage: &str,
        sha: &str,
        now_ms: i64,
    ) -> Result<usize> {
        Ok(self.conn().execute(
            "UPDATE approval_push_nonces SET used_at_ms=?4 \
             WHERE issue_id=?1 AND stage=?2 AND commit_sha=?3 AND used_at_ms IS NULL",
            params![issue, stage, sha, now_ms],
        )?)
    }

    /// Retention: settled proposals older than 30 days and nonces older than
    /// 2 days. Pending proposals are never pruned here (expiry marks them).
    pub(crate) fn prune_approval_effects(&self, now_ms: i64) -> Result<usize> {
        let proposals = self.conn().execute(
            "DELETE FROM automation_proposals WHERE state <> 'pending' AND created_at_ms < ?1",
            params![now_ms.saturating_sub(PROPOSAL_RETENTION_MS)],
        )?;
        let nonces = self.conn().execute(
            "DELETE FROM approval_push_nonces WHERE created_at_ms < ?1",
            params![now_ms.saturating_sub(NONCE_RETENTION_MS)],
        )?;
        Ok(proposals + nonces)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(sha: &str, created: i64) -> ProposalRow {
        ProposalRow {
            id: 0,
            approval_issue: "linear:THE-1".into(),
            approval_stage: "start.coder".into(),
            approval_sha: sha.into(),
            rule_id: "lead:x".into(),
            repo_root: "/r".into(),
            issue_id: "linear:THE-1".into(),
            provider: "linear".into(),
            account: "work".into(),
            stage: "coder".into(),
            base_sha: "abc1234".into(),
            created_at_ms: created,
            expires_at_ms: created + 1_000,
            state: "pending".into(),
        }
    }

    #[test]
    fn proposal_put_is_idempotent_and_reads_back() {
        let db = Db::open_memory().unwrap();
        assert!(db.put_proposal(&row("aaaaaaa", 1)).unwrap());
        assert!(!db.put_proposal(&row("aaaaaaa", 2)).unwrap(), "same triple");
        let got = db
            .proposal_for("linear:THE-1", "start.coder", "aaaaaaa")
            .unwrap()
            .unwrap();
        assert_eq!(got.created_at_ms, 1, "the first write wins");
        assert_eq!(got.state, "pending");
        assert!(
            db.proposal_for("linear:THE-1", "start.coder", "bbbbbbb")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn proposal_state_is_compare_and_set() {
        let db = Db::open_memory().unwrap();
        db.put_proposal(&row("aaaaaaa", 1)).unwrap();
        let id = db
            .proposal_for("linear:THE-1", "start.coder", "aaaaaaa")
            .unwrap()
            .unwrap()
            .id;
        assert!(db.set_proposal_state(id, "pending", "started").unwrap());
        assert!(
            !db.set_proposal_state(id, "pending", "rejected").unwrap(),
            "lost the race"
        );
        assert!(
            !db.set_proposal_state(id + 9, "started", "failed").unwrap(),
            "missing row"
        );
        assert!(db.set_proposal_state(id, "started", "failed").unwrap());
        assert!(
            db.set_proposal_state(id, "failed", "bogus").is_err(),
            "CHECK holds"
        );
    }

    #[test]
    fn nonce_is_single_use_and_expires() {
        let db = Db::open_memory().unwrap();
        db.put_push_nonce("h1", "linear:THE-1", "coder", "abc1234", 100, 200)
            .unwrap();
        db.put_push_nonce("h2", "linear:THE-2", "coder", "def5678", 100, 200)
            .unwrap();
        assert!(
            db.put_push_nonce("h1", "x", "y", "z", 1, 2).is_err(),
            "hash is unique"
        );
        assert_eq!(
            db.consume_push_nonce("h1", 150).unwrap(),
            Some(("linear:THE-1".into(), "coder".into(), "abc1234".into()))
        );
        assert_eq!(
            db.consume_push_nonce("h1", 151).unwrap(),
            None,
            "single use"
        );
        assert_eq!(
            db.consume_push_nonce("h2", 200).unwrap(),
            None,
            "expired at the bound"
        );
        assert_eq!(db.consume_push_nonce("nope", 1).unwrap(), None);
    }

    #[test]
    fn voiding_a_triple_burns_only_its_nonces() {
        let db = Db::open_memory().unwrap();
        db.put_push_nonce("a1", "linear:THE-1", "coder", "abc1234", 100, 1_000)
            .unwrap();
        db.put_push_nonce("a2", "linear:THE-1", "coder", "abc1234", 110, 1_000)
            .unwrap();
        db.put_push_nonce("b1", "linear:THE-2", "coder", "abc1234", 100, 1_000)
            .unwrap();
        assert_eq!(
            db.void_push_nonces("linear:THE-1", "coder", "abc1234", 150)
                .unwrap(),
            2
        );
        assert_eq!(db.consume_push_nonce("a1", 160).unwrap(), None);
        assert_eq!(db.consume_push_nonce("a2", 160).unwrap(), None);
        assert_eq!(
            db.consume_push_nonce("b1", 160).unwrap(),
            Some(("linear:THE-2".into(), "coder".into(), "abc1234".into()))
        );
    }

    #[test]
    fn retention_prunes_settled_proposals_and_old_nonces() {
        let db = Db::open_memory().unwrap();
        let now = PROPOSAL_RETENTION_MS * 2;
        let old = now - PROPOSAL_RETENTION_MS - 1;
        db.put_proposal(&row("aaaaaaa", old)).unwrap();
        db.put_proposal(&row("bbbbbbb", old)).unwrap();
        db.put_proposal(&row("ccccccc", now)).unwrap();
        let settled = db
            .proposal_for("linear:THE-1", "start.coder", "bbbbbbb")
            .unwrap()
            .unwrap();
        db.set_proposal_state(settled.id, "pending", "rejected")
            .unwrap();
        db.put_push_nonce(
            "old",
            "i",
            "s",
            "abc1234",
            now - NONCE_RETENTION_MS - 1,
            now,
        )
        .unwrap();
        db.put_push_nonce("new", "i", "s", "abc1234", now, now + 1)
            .unwrap();
        assert_eq!(db.prune_approval_effects(now).unwrap(), 2);
        assert!(
            db.proposal_for("linear:THE-1", "start.coder", "aaaaaaa")
                .unwrap()
                .is_some(),
            "pending kept"
        );
        assert!(
            db.proposal_for("linear:THE-1", "start.coder", "bbbbbbb")
                .unwrap()
                .is_none()
        );
        assert!(db.consume_push_nonce("new", now).unwrap().is_some());
        assert!(db.consume_push_nonce("old", 0).unwrap().is_none());
    }
}
