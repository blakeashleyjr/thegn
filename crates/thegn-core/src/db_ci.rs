//! SQLite bodies for the CI log cache and autofix dedupe marker.

use anyhow::Result;
use rusqlite::{OptionalExtension, params};

use crate::ci_log::{
    CiLogCandidate, CiLogEntry, HARD_MAX_LOG_BYTES, HARD_MAX_LOG_LINES, REDACTION_POLICY_VERSION,
};
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
                    let version = r.get::<_, i64>(7)?;
                    Ok(CiLogEntry {
                        worktree: r.get(0)?,
                        run_id: r.get(1)?,
                        job_id: r.get(2)?,
                        job_name: r.get(3)?,
                        head_sha: r.get(4)?,
                        text: r.get(5)?,
                        truncated: r.get::<_, i64>(6)? != 0,
                        redacted: version != 0,
                        policy_version: u32::try_from(version).unwrap_or(0),
                        fetched_at: r.get(8)?,
                    }
                    .revalidated())
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
            let version = r.get::<_, i64>(7)?;
            Ok(CiLogEntry {
                worktree: r.get(0)?,
                run_id: r.get(1)?,
                job_id: r.get(2)?,
                job_name: r.get(3)?,
                head_sha: r.get(4)?,
                text: r.get(5)?,
                truncated: r.get::<_, i64>(6)? != 0,
                redacted: version != 0,
                policy_version: u32::try_from(version).unwrap_or(0),
                fetched_at: r.get(8)?,
            })
        })?;
        Ok(rows
            .map(|row| row.map(CiLogEntry::revalidated))
            .collect::<rusqlite::Result<Vec<_>>>()?)
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
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)
             ON CONFLICT(worktree, run_id, job_id) DO UPDATE SET
               job_name=?4, head_sha=?5, text=?6, truncated=?7, redacted=?8, fetched_at=?9",
            params![
                entry.worktree,
                entry.run_id,
                entry.job_id,
                entry.job_name,
                entry.head_sha,
                text,
                i64::from(entry.truncated || hard_truncated),
                i64::from(REDACTION_POLICY_VERSION),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ci_log::{HARD_MAX_LOG_BYTES, HARD_MAX_LOG_LINES, REDACTION_MARKER};
    use crate::store::CacheStore;

    fn raw_entry(text: String) -> CiLogEntry {
        let mut entry = CiLogEntry::new("/wt/ci", "run", "job", "tests", "ordinary\n", 20, 1024, 1);
        entry.text = text;
        entry
    }

    fn insert_policy_row(db: &Db, run_id: &str, version: i64, text: &str) {
        db.conn()
            .execute(
                "INSERT INTO ci_log_cache
                 (worktree, run_id, job_id, job_name, head_sha, text, truncated, redacted, fetched_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0, ?7, 1)",
                rusqlite::params!["/wt/ci", run_id, "job", "tests", "", text, version],
            )
            .unwrap();
    }

    #[test]
    fn policy_version_legacy_row_is_re_redacted_on_read() {
        let db = Db::open_memory().unwrap();
        let gh_token = format!("ghp_{}", "7".repeat(36));
        insert_policy_row(
            &db,
            "legacy",
            1,
            &format!("token=SYNTHETIC_TOKEN\n{gh_token}\nordinary\n"),
        );

        let single = db.get_ci_log("/wt/ci", "legacy", "job").unwrap().unwrap();
        let listed = db.list_ci_logs("/wt/ci").unwrap().remove(0);
        for entry in [single, listed] {
            assert!(!entry.text.contains("SYNTHETIC_TOKEN"));
            assert!(!entry.text.contains(&gh_token));
            assert!(entry.text.contains("ordinary"));
            assert_eq!(entry.policy_version, REDACTION_POLICY_VERSION);
            assert!(entry.redacted);
        }
    }

    #[test]
    fn policy_version_is_stored_with_each_row() {
        let db = Db::open_memory().unwrap();
        let entry = CiLogEntry::new(
            "/wt/ci",
            "versioned",
            "job",
            "tests",
            "ordinary\n",
            10,
            100,
            1,
        );
        db.put_ci_log(&entry).unwrap();
        let stored_version = db
            .conn()
            .query_row(
                "SELECT redacted FROM ci_log_cache WHERE worktree=?1 AND run_id=?2 AND job_id=?3",
                rusqlite::params!["/wt/ci", "versioned", "job"],
                |row| row.get::<_, i64>(0),
            )
            .unwrap();
        let read_back = db
            .get_ci_log("/wt/ci", "versioned", "job")
            .unwrap()
            .unwrap();

        assert_eq!(stored_version, i64::from(REDACTION_POLICY_VERSION));
        assert_eq!(read_back.policy_version, REDACTION_POLICY_VERSION);
    }

    #[test]
    fn policy_version_negative_column_value_is_re_redacted() {
        let db = Db::open_memory().unwrap();
        insert_policy_row(&db, "negative", -1, "token=SYNTHETIC_TOKEN\nordinary\n");

        let entry = db.get_ci_log("/wt/ci", "negative", "job").unwrap().unwrap();
        assert!(!entry.text.contains("SYNTHETIC_TOKEN"));
        assert!(entry.text.contains("ordinary"));
        assert_eq!(entry.policy_version, REDACTION_POLICY_VERSION);
    }

    #[test]
    fn store_redacts_raw_entries_that_bypassed_the_constructor() {
        let db = Db::open_memory().unwrap();
        let begin = format!("-----{} PRIVATE KEY-----", "BEGIN");
        let end = format!("-----{} PRIVATE KEY-----", "END");
        let gh_token = format!("ghp_{}", "7".repeat(36));
        let body = "SYNTHETIC_PEM_BODY";
        let entry = raw_entry(format!(
            "{begin}\n{body}\n{end}\ntoken=SYNTHETIC_TOKEN\n{gh_token}\n"
        ));

        db.put_ci_log(&entry).unwrap();
        let stored = db.get_ci_log("/wt/ci", "run", "job").unwrap().unwrap();
        let listed = db.list_ci_logs("/wt/ci").unwrap().remove(0);
        for row in [stored, listed] {
            assert!(!row.text.contains(body));
            assert!(!row.text.contains("SYNTHETIC_TOKEN"));
            assert!(!row.text.contains(&gh_token));
            assert!(row.text.contains(REDACTION_MARKER));
            assert!(row.redacted);
        }
    }

    #[test]
    fn store_enforces_hard_bounds_at_maximum_ingestion() {
        let db = Db::open_memory().unwrap();
        let line_input = (0..HARD_MAX_LOG_LINES + 10)
            .map(|line| format!("line-{line}\n"))
            .collect::<String>();
        let mut lines = raw_entry(line_input);
        lines.run_id = "many-lines".into();
        db.put_ci_log(&lines).unwrap();
        let stored_lines = db
            .get_ci_log("/wt/ci", "many-lines", "job")
            .unwrap()
            .unwrap();
        assert!(stored_lines.truncated);
        assert!(stored_lines.text.lines().count() <= HARD_MAX_LOG_LINES);
        assert!(
            stored_lines
                .text
                .ends_with(&format!("line-{}\n", HARD_MAX_LOG_LINES + 9))
        );

        let expanded_input = (0..HARD_MAX_LOG_LINES)
            .map(|line| {
                if line + 1 == HARD_MAX_LOG_LINES {
                    "token=a padding_padding_padding_padding latest\n".to_string()
                } else {
                    "token=a padding_padding_padding_padding\n".to_string()
                }
            })
            .collect::<String>();
        let mut bytes = raw_entry(expanded_input);
        bytes.run_id = "expanded-bytes".into();
        db.put_ci_log(&bytes).unwrap();
        let stored_bytes = db
            .get_ci_log("/wt/ci", "expanded-bytes", "job")
            .unwrap()
            .unwrap();
        assert!(stored_bytes.truncated);
        assert!(stored_bytes.text.len() <= HARD_MAX_LOG_BYTES);
        assert!(stored_bytes.text.lines().count() <= HARD_MAX_LOG_LINES);
        assert!(stored_bytes.text.ends_with("latest\n"));
    }
}
