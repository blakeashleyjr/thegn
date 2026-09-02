//! SQLite bodies for the CI log cache and autofix dedupe marker.

use anyhow::Result;
use rusqlite::{OptionalExtension, params};

use crate::ci_log::{CiLogCandidate, CiLogEntry, HARD_MAX_LOG_BYTES, HARD_MAX_LOG_LINES};
use crate::db::Db;
use crate::util;

impl Db {
    pub(crate) fn get_ci_log_entry(
        &self,
        worktree: &str,
        run_id: &str,
        job_id: &str,
    ) -> Result<Option<CiLogEntry>> {
        let row = self
            .conn()
            .query_row(
                "SELECT worktree, run_id, job_id, job_name, head_sha, text, truncated, redacted, fetched_at
                 FROM ci_log_cache WHERE worktree=?1 AND run_id=?2 AND job_id=?3",
                params![worktree, run_id, job_id],
                |r| {
                    Ok(CiLogEntry {
                        worktree: r.get(0)?,
                        run_id: r.get(1)?,
                        job_id: r.get(2)?,
                        job_name: r.get(3)?,
                        head_sha: r.get(4)?,
                        text: r.get(5)?,
                        truncated: r.get::<_, i64>(6)? != 0,
                        redacted: r.get::<_, i64>(7)? != 0,
                        fetched_at: r.get(8)?,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    pub(crate) fn list_ci_log_entries(&self, worktree: &str) -> Result<Vec<CiLogEntry>> {
        let mut stmt = self.conn().prepare(
            "SELECT worktree, run_id, job_id, job_name, head_sha, text, truncated, redacted, fetched_at
             FROM ci_log_cache WHERE worktree=?1 ORDER BY fetched_at DESC, run_id, job_id",
        )?;
        let rows = stmt.query_map(params![worktree], |r| {
            Ok(CiLogEntry {
                worktree: r.get(0)?,
                run_id: r.get(1)?,
                job_id: r.get(2)?,
                job_name: r.get(3)?,
                head_sha: r.get(4)?,
                text: r.get(5)?,
                truncated: r.get::<_, i64>(6)? != 0,
                redacted: r.get::<_, i64>(7)? != 0,
                fetched_at: r.get(8)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub(crate) fn put_ci_log_entry(&self, entry: &CiLogEntry) -> Result<()> {
        // A store implementation is a second defense for callers that did not
        // use CiLogEntry::new: public text is redacted and hard-capped before it
        // reaches SQLite.
        let redacted = crate::ci_log::redact(&entry.text);
        let (text, hard_truncated) =
            crate::ci_log::bounded_tail(&redacted, HARD_MAX_LOG_LINES, HARD_MAX_LOG_BYTES);
        self.conn().execute(
            "INSERT INTO ci_log_cache
             (worktree, run_id, job_id, job_name, head_sha, text, truncated, redacted, fetched_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,1,?8)
             ON CONFLICT(worktree, run_id, job_id) DO UPDATE SET
               job_name=?4, head_sha=?5, text=?6, truncated=?7, redacted=1, fetched_at=?8",
            params![
                entry.worktree,
                entry.run_id,
                entry.job_id,
                entry.job_name,
                entry.head_sha,
                text,
                i64::from(entry.truncated || hard_truncated),
                if entry.fetched_at == 0 {
                    util::now()
                } else {
                    entry.fetched_at
                }
            ],
        )?;
        Ok(())
    }

    pub(crate) fn retain_ci_log_runs(&self, worktree: &str, run_ids: &[String]) -> Result<usize> {
        if run_ids.is_empty() {
            return Ok(self.conn().execute(
                "DELETE FROM ci_log_cache WHERE worktree=?1",
                params![worktree],
            )?);
        }
        let placeholders = std::iter::repeat_n("?", run_ids.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "DELETE FROM ci_log_cache WHERE worktree=?1 AND run_id NOT IN ({placeholders})"
        );
        let mut values: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(run_ids.len() + 1);
        values.push(&worktree);
        for id in run_ids {
            values.push(id);
        }
        Ok(self.conn().execute(&sql, values.as_slice())?)
    }

    pub(crate) fn claim_ci_autofix(&self, candidate: &CiLogCandidate) -> Result<bool> {
        let inserted = self.conn().execute(
            "INSERT OR IGNORE INTO ci_autofix_dedupe
             (worktree, run_id, job_id, head_sha, claimed_at) VALUES (?1,?2,?3,?4,?5)",
            params![
                candidate.worktree,
                candidate.run_id,
                candidate.job_id,
                candidate.head_sha,
                util::now()
            ],
        )?;
        Ok(inserted == 1)
    }
}
