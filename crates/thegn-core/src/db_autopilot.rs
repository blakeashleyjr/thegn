//! SQLite implementation of the issue-autopilot claim journal.

use crate::autopilot::{AutopilotIssueKey, AutopilotState, AutopilotSummary, bounded_reason};
use crate::db::Db;
use crate::store::{AutopilotStore, ClaimOutcome};
use anyhow::Result;
use rusqlite::{OptionalExtension, params};

const ACTIVE_STATES: &str = "('claimed','working','pr_opened','shepherding')";

fn map_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<AutopilotSummary> {
    Ok(AutopilotSummary {
        id: row.get(0)?,
        key: AutopilotIssueKey::new(
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
        ),
        repo_root: row.get(4)?,
        worktree: row.get(5)?,
        branch: row.get(6)?,
        base_branch: row.get(7)?,
        state: AutopilotState::parse(&row.get::<_, String>(8)?),
        attempt: row.get::<_, i64>(9)?.max(0) as u32,
        dispatch_id: row.get(10)?,
        pr_number: row.get::<_, Option<i64>>(11)?.map(|n| n as u64),
        pr_head: row.get(12)?,
        pr_url: row.get(13)?,
        created_at: row.get(14)?,
        updated_at: row.get(15)?,
        claimed_at: row.get(16)?,
        finished_at: row.get(17)?,
        reason: row.get(18)?,
    })
}

const SELECT: &str = "SELECT id,provider,account,issue_id,repo_root,worktree,branch,base_branch,state,attempt,dispatch_id,pr_number,pr_head,pr_url,created_at,updated_at,claimed_at,finished_at,last_reason FROM autopilot_runs";

impl AutopilotStore for Db {
    fn claim_autopilot(
        &self,
        key: &AutopilotIssueKey,
        repo_root: &str,
        max_concurrent: u32,
        max_attempts: u32,
        now: i64,
    ) -> Result<ClaimOutcome> {
        self.conn().execute_batch("BEGIN IMMEDIATE")?;
        let result = (|| -> Result<ClaimOutcome> {
            let active: i64 = self.conn().query_row(
                &format!("SELECT COUNT(*) FROM autopilot_runs WHERE repo_root=?1 AND state IN {ACTIVE_STATES}"),
                [repo_root], |row| row.get(0))?;
            if active >= max_concurrent as i64 {
                return Ok(ClaimOutcome::AtCapacity);
            }
            let prior: Option<i64> = self.conn().query_row(
                "SELECT attempt FROM autopilot_runs WHERE provider=?1 AND account=?2 AND issue_id=?3",
                params![key.provider, key.account, key.issue_id], |row| row.get(0)).optional()?;
            if let Some(attempt) = prior {
                return Ok(if attempt >= max_attempts as i64 {
                    ClaimOutcome::AttemptsExhausted
                } else {
                    ClaimOutcome::AlreadyClaimed
                });
            }
            let inserted = self.conn().execute(
                "INSERT INTO autopilot_runs (provider,account,issue_id,repo_root,state,attempt,created_at,updated_at,claimed_at) VALUES (?1,?2,?3,?4,'claimed',1,?5,?5,?5) ON CONFLICT(provider,account,issue_id) DO NOTHING",
                params![key.provider, key.account, key.issue_id, repo_root, now])?;
            if inserted != 1 {
                return Ok(ClaimOutcome::AlreadyClaimed);
            }
            let id = self.conn().last_insert_rowid();
            Ok(ClaimOutcome::Claimed(Box::new(
                self.get_autopilot_run(id)?
                    .expect("inserted autopilot run disappeared"),
            )))
        })();
        match result {
            Ok(value) => {
                self.conn().execute_batch("COMMIT")?;
                Ok(value)
            }
            Err(error) => {
                let _ = self.conn().execute_batch("ROLLBACK");
                Err(error)
            }
        }
    }

    fn get_autopilot_run(&self, id: i64) -> Result<Option<AutopilotSummary>> {
        let mut stmt = self.conn().prepare(&format!("{SELECT} WHERE id=?1"))?;
        Ok(stmt.query_row([id], map_row).optional()?)
    }

    fn list_autopilot_runs(&self, repo_root: &str, limit: usize) -> Result<Vec<AutopilotSummary>> {
        let mut stmt = self.conn().prepare(&format!(
            "{SELECT} WHERE repo_root=?1 ORDER BY updated_at DESC, id DESC LIMIT ?2"
        ))?;
        let rows = stmt.query_map(params![repo_root, limit.min(1_000) as i64], map_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    fn transition_autopilot(
        &self,
        id: i64,
        expected: AutopilotState,
        next: AutopilotState,
        reason: Option<&str>,
        pr_number: Option<u64>,
        now: i64,
    ) -> Result<bool> {
        let reason = bounded_reason(reason);
        let finished = next.is_terminal().then_some(now);
        let changed = self.conn().execute(
            "UPDATE autopilot_runs SET state=?2,updated_at=?3,last_reason=COALESCE(?4,last_reason),pr_number=COALESCE(?5,pr_number),finished_at=COALESCE(?6,finished_at) WHERE id=?1 AND state=?7",
            params![id, next.as_str(), now, reason, pr_number.map(|n| n as i64), finished, expected.as_str()])?;
        Ok(changed == 1)
    }

    fn attach_autopilot_dispatch(&self, id: i64, dispatch_id: i64, now: i64) -> Result<bool> {
        Ok(self.conn().execute(
            "UPDATE autopilot_runs SET dispatch_id=?2,updated_at=?3 WHERE id=?1",
            params![id, dispatch_id, now],
        )? == 1)
    }

    fn set_autopilot_worktree(
        &self,
        id: i64,
        worktree: &str,
        branch: &str,
        base_branch: &str,
        now: i64,
    ) -> Result<bool> {
        Ok(self.conn().execute("UPDATE autopilot_runs SET worktree=?2,branch=?3,base_branch=?4,updated_at=?5 WHERE id=?1", params![id, worktree, branch, base_branch, now])? == 1)
    }

    fn set_autopilot_pr(
        &self,
        id: i64,
        number: u64,
        head: &str,
        url: &str,
        now: i64,
    ) -> Result<bool> {
        Ok(self.conn().execute(
            "UPDATE autopilot_runs SET pr_number=?2,pr_head=?3,pr_url=?4,updated_at=?5 WHERE id=?1",
            params![id, number as i64, head, url, now],
        )? == 1)
    }

    fn find_autopilot_by_pr(
        &self,
        repo_root: &str,
        number: u64,
    ) -> Result<Option<AutopilotSummary>> {
        let mut stmt = self.conn().prepare(&format!(
            "{SELECT} WHERE repo_root=?1 AND pr_number=?2 ORDER BY id DESC LIMIT 1"
        ))?;
        Ok(stmt
            .query_row(params![repo_root, number as i64], map_row)
            .optional()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    #[test]
    fn duplicate_claim_and_readback_are_durable() {
        let db = Db::open_memory().unwrap();
        let key = AutopilotIssueKey::new("linear", "work", "THE-56");
        let first = db.claim_autopilot(&key, "/repo", 1, 1, 42).unwrap();
        let id = match first {
            ClaimOutcome::Claimed(row) => row.id,
            other => panic!("{other:?}"),
        };
        assert!(matches!(
            db.claim_autopilot(&key, "/repo", 1, 1, 43).unwrap(),
            ClaimOutcome::AtCapacity
                | ClaimOutcome::AttemptsExhausted
                | ClaimOutcome::AlreadyClaimed
        ));
        let row = db.get_autopilot_run(id).unwrap().unwrap();
        assert_eq!(row.key, key);
        assert_eq!(row.attempt, 1);
        assert!(
            db.transition_autopilot(
                id,
                AutopilotState::Claimed,
                AutopilotState::Working,
                Some("started"),
                None,
                44
            )
            .unwrap()
        );
        assert!(
            !db.transition_autopilot(
                id,
                AutopilotState::Claimed,
                AutopilotState::NeedsHuman,
                Some("stale"),
                None,
                45
            )
            .unwrap()
        );
    }
}
