//! SQLite bodies for the CI log cache and autofix dedupe marker.

use anyhow::Result;
use rusqlite::{OptionalExtension, params};

use crate::ci_log::{
    CiLogCandidate, CiLogEntry, HARD_MAX_LOG_BYTES, HARD_MAX_LOG_LINES, REDACTION_POLICY_VERSION,
};
use crate::db::Db;
use crate::util;

/// Claim a candidate once per (worktree, run, job, head, attempt). Empty legacy or
/// unknown contexts never replace or reclaim an existing dedupe row; unknown attempt = -1.
pub(crate) const CLAIM_CI_AUTOFIX_SQL: &str = "INSERT INTO ci_autofix_dedupe (worktree, run_id, job_id, head_sha, claimed_at, context, attempt) VALUES (?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(worktree, run_id, job_id, head_sha, attempt) DO UPDATE SET context=excluded.context, claimed_at=excluded.claimed_at WHERE ci_autofix_dedupe.context <> '' AND excluded.context <> '' AND ci_autofix_dedupe.context <> excluded.context";

/// The stored attempt key of a candidate: the known provider run attempt, or -1 when unknown.
pub(crate) fn ci_autofix_attempt_key(candidate: &CiLogCandidate) -> i64 {
    candidate
        .attempt
        .and_then(|attempt| i64::try_from(attempt).ok())
        .unwrap_or(-1)
}

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
                "SELECT worktree, run_id, job_id, job_name, head_sha, text, truncated, redacted, fetched_at, context, attempt, redaction_changed
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
                        context: r.get(9)?,
                        attempt: r.get::<_, Option<i64>>(10)?.and_then(|n| u64::try_from(n).ok()),
                        redaction_changed: r.get::<_, Option<i64>>(11)?.map(|n| n != 0),
                    }
                    .revalidated())
                },
            )
            .optional()?;
        Ok(row)
    }

    pub(crate) fn list_ci_log_entries(&self, worktree: &str) -> Result<Vec<CiLogEntry>> {
        let mut stmt = self.conn().prepare(
            "SELECT worktree, run_id, job_id, job_name, head_sha, text, truncated, redacted, fetched_at, context, attempt, redaction_changed
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
                context: r.get(9)?,
                attempt: r
                    .get::<_, Option<i64>>(10)?
                    .and_then(|n| u64::try_from(n).ok()),
                redaction_changed: r.get::<_, Option<i64>>(11)?.map(|n| n != 0),
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
        let changed = if redacted != entry.text {
            Some(true)
        } else {
            entry.redaction_changed
        };
        let (text, hard_truncated) =
            crate::ci_log::bounded_tail(&redacted, HARD_MAX_LOG_LINES, HARD_MAX_LOG_BYTES);
        self.conn().execute(
            "INSERT INTO ci_log_cache
             (worktree, run_id, job_id, job_name, head_sha, text, truncated, redacted, fetched_at, context, attempt, redaction_changed)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)
             ON CONFLICT(worktree, run_id, job_id) DO UPDATE SET
               job_name=?4, head_sha=?5, text=?6, truncated=?7, redacted=?8, fetched_at=?9, context=?10, attempt=?11, redaction_changed=?12",
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
                },
                entry.context,
                entry.attempt.and_then(|n| i64::try_from(n).ok()),
                changed.map(i64::from)
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
            CLAIM_CI_AUTOFIX_SQL,
            params![
                candidate.worktree,
                candidate.run_id,
                candidate.job_id,
                candidate.head_sha,
                util::now(),
                candidate.context,
                ci_autofix_attempt_key(candidate)
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

    #[test]
    // Positive control: context changes still retain their established dedupe behavior.
    fn ci_autofix_claim_is_keyed_by_provider_context() {
        let db = Db::open_memory().unwrap();
        let a = CiLogCandidate {
            worktree: "/wt/a".into(),
            run_id: "r".into(),
            job_id: "j".into(),
            head_sha: "sha".into(),
            context: "github:github.com/o/a".into(),
            attempt: None,
        };
        assert!(db.claim_ci_autofix(&a).unwrap());
        assert!(!db.claim_ci_autofix(&a).unwrap());
        let b = CiLogCandidate {
            context: "github:github.com/o/b".into(),
            ..a.clone()
        };
        assert!(db.claim_ci_autofix(&b).unwrap());
        assert!(!db.claim_ci_autofix(&b).unwrap());
    }

    #[test]
    // Positive control: unknown provider contexts still cannot reclaim the same attempt key.
    fn ci_autofix_claim_never_reclaims_an_unknown_context() {
        let db = Db::open_memory().unwrap();
        let unknown = CiLogCandidate {
            worktree: "/wt/a".into(),
            run_id: "r1".into(),
            job_id: "j".into(),
            head_sha: "sha".into(),
            context: String::new(),
            attempt: None,
        };
        assert!(db.claim_ci_autofix(&unknown).unwrap());
        let known = CiLogCandidate {
            context: "github:github.com/o/a".into(),
            ..unknown.clone()
        };
        assert!(!db.claim_ci_autofix(&known).unwrap());
        let known_first = CiLogCandidate {
            run_id: "r2".into(),
            context: "github:github.com/o/a".into(),
            ..unknown
        };
        assert!(db.claim_ci_autofix(&known_first).unwrap());
        let unknown_second = CiLogCandidate {
            context: String::new(),
            ..known_first
        };
        assert!(!db.claim_ci_autofix(&unknown_second).unwrap());
    }

    #[test]
    fn ci_autofix_claim_is_keyed_by_run_attempt() {
        let db = Db::open_memory().unwrap();
        let a = CiLogCandidate {
            worktree: "/wt/a".into(),
            run_id: "r".into(),
            job_id: "j".into(),
            head_sha: "sha".into(),
            context: "github:github.com/o/a".into(),
            attempt: Some(1),
        };
        assert!(db.claim_ci_autofix(&a).unwrap());
        assert!(!db.claim_ci_autofix(&a).unwrap());
        let next = CiLogCandidate {
            attempt: Some(2),
            ..a.clone()
        };
        assert!(db.claim_ci_autofix(&next).unwrap());
        assert!(!db.claim_ci_autofix(&next).unwrap());
        assert!(!db.claim_ci_autofix(&a).unwrap());
    }

    #[test]
    fn ci_autofix_unknown_attempt_is_a_separate_key_from_a_known_attempt() {
        let db = Db::open_memory().unwrap();
        let unknown = CiLogCandidate {
            worktree: "/wt/a".into(),
            run_id: "r".into(),
            job_id: "j".into(),
            head_sha: "sha".into(),
            context: "github:github.com/o/a".into(),
            attempt: None,
        };
        assert!(db.claim_ci_autofix(&unknown).unwrap());
        assert!(!db.claim_ci_autofix(&unknown).unwrap());
        let known = CiLogCandidate {
            attempt: Some(1),
            ..unknown.clone()
        };
        assert!(db.claim_ci_autofix(&known).unwrap());
        assert!(!db.claim_ci_autofix(&known).unwrap());
        assert!(!db.claim_ci_autofix(&unknown).unwrap());
        assert_eq!(ci_autofix_attempt_key(&unknown), -1);
        assert_eq!(
            ci_autofix_attempt_key(&CiLogCandidate {
                attempt: Some(7),
                ..unknown.clone()
            }),
            7
        );
        assert_eq!(
            ci_autofix_attempt_key(&CiLogCandidate {
                attempt: Some(u64::MAX),
                ..unknown
            }),
            -1
        );
    }

    #[test]
    fn ci_log_cache_round_trips_the_provider_context() {
        let db = Db::open_memory().unwrap();
        let mut entry = CiLogEntry::new("/wt/a", "run", "job", "tests", "log", 10, 1024, 1);
        entry.head_sha = "sha".into();
        entry.context = "github:github.com/o/a".into();
        db.put_ci_log(&entry).unwrap();
        assert_eq!(
            db.get_ci_log("/wt/a", "run", "job")
                .unwrap()
                .unwrap()
                .context,
            entry.context
        );
        assert_eq!(db.list_ci_logs("/wt/a").unwrap()[0].context, entry.context);
        entry.context = "github:github.com/o/b".into();
        db.put_ci_log(&entry).unwrap();
        assert_eq!(
            db.get_ci_log("/wt/a", "run", "job")
                .unwrap()
                .unwrap()
                .context,
            entry.context
        );
    }

    #[test]
    fn ci_log_cache_round_trips_attempt_and_redaction_changed() {
        let db = Db::open_memory().unwrap();
        let mut entry = CiLogEntry::new(
            "/wt/attempt",
            "run",
            "job",
            "tests",
            "ordinary\n",
            10,
            1024,
            1,
        );
        entry.attempt = Some(3);
        entry.redaction_changed = Some(false);
        db.put_ci_log(&entry).unwrap();
        let got = db.get_ci_log("/wt/attempt", "run", "job").unwrap().unwrap();
        assert_eq!(got.attempt, Some(3));
        assert_eq!(got.redaction_changed, Some(false));
        let listed = db.list_ci_logs("/wt/attempt").unwrap();
        assert_eq!(listed[0].attempt, Some(3));
        assert_eq!(listed[0].redaction_changed, Some(false));

        entry.attempt = Some(4);
        db.put_ci_log(&entry).unwrap();
        let listed = db.list_ci_logs("/wt/attempt").unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].attempt, Some(4));

        entry.attempt = None;
        entry.run_id = "unknown".into();
        db.put_ci_log(&entry).unwrap();
        assert_eq!(
            db.get_ci_log("/wt/attempt", "unknown", "job")
                .unwrap()
                .unwrap()
                .attempt,
            None
        );
        db.conn()
            .execute(
                "UPDATE ci_log_cache SET attempt=-1 WHERE worktree=?1 AND run_id=?2 AND job_id=?3",
                rusqlite::params!["/wt/attempt", "unknown", "job"],
            )
            .unwrap();
        assert_eq!(
            db.get_ci_log("/wt/attempt", "unknown", "job")
                .unwrap()
                .unwrap()
                .attempt,
            None
        );
    }

    #[test]
    fn ci_log_put_marks_a_raw_secret_row_as_redaction_changed() {
        let db = Db::open_memory().unwrap();
        let mut entry =
            CiLogEntry::new("/wt/raw", "run", "job", "tests", "ordinary\n", 10, 1024, 1);
        entry.text = "token=SYNTHETIC\nordinary\n".into();
        entry.redaction_changed = Some(false);
        db.put_ci_log(&entry).unwrap();
        let stored = db.get_ci_log("/wt/raw", "run", "job").unwrap().unwrap();
        assert_eq!(stored.redaction_changed, Some(true));
        assert!(!stored.text.contains("SYNTHETIC"));
    }
}
