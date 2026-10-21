//! v52: the calendar cache — `calendar_events` (per-account events) and
//! `calendar_sync` (per-account cursor + last-fetch bookkeeping). SQLite impl
//! of [`crate::store::CalendarStore`].
//!
//! Row-per-event rather than one JSON blob per account, deliberately departing
//! from the `pr_cache`/`issue_cache` shape:
//!
//! 1. an incremental sync delivers deltas and tombstones, and a blob would
//!    force a full refetch to apply a single deletion;
//! 2. showing one month should not deserialize a year of events;
//! 3. a "next event" bar widget wants one indexed row, not a whole calendar.

use std::collections::BTreeSet;

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};

use crate::db::Db;
use crate::store::{
    CalendarCursor, CalendarPageApplied, CalendarPageWrite, CalendarRow, CalendarStore,
    CalendarSyncRow, CalendarTombstones, EmptySnapshot,
};
use crate::util;

/// Create the calendar cache tables. Schema v84 rebuilds the calendar key to
/// include the recurrence instance id.
pub(crate) fn migrate_v52(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS calendar_events (
            account    TEXT NOT NULL,
            uid        TEXT NOT NULL,
            calendar   TEXT NOT NULL DEFAULT '',
            start_ms   INTEGER NOT NULL,
            end_ms     INTEGER NOT NULL,
            recurring  INTEGER NOT NULL DEFAULT 0,
            json       TEXT NOT NULL,
            fetched_at INTEGER NOT NULL,
            PRIMARY KEY (account, uid)
        );
        CREATE INDEX IF NOT EXISTS calendar_events_span
            ON calendar_events(start_ms, end_ms);
        CREATE INDEX IF NOT EXISTS calendar_events_recurring
            ON calendar_events(recurring);

        CREATE TABLE IF NOT EXISTS calendar_sync (
            account         TEXT PRIMARY KEY,
            provider        TEXT NOT NULL DEFAULT '',
            sync_token      TEXT NOT NULL DEFAULT '',
            fetched_at      INTEGER NOT NULL DEFAULT 0,
            last_error      TEXT NOT NULL DEFAULT '',
            horizon_from_ms INTEGER NOT NULL DEFAULT 0,
            horizon_to_ms   INTEGER NOT NULL DEFAULT 0
        );
        "#,
    )?;
    Ok(())
}

impl CalendarStore for Db {
    fn get_calendar_events(
        &self,
        from_ms: i64,
        to_ms: i64,
        accounts: &[String],
    ) -> Result<Vec<(String, String)>> {
        // `recurring = 1 OR overlaps` keeps the range query honest: a weekly
        // master from two years ago still produces occurrences in this month,
        // so it must be loaded and expanded even though its own span misses.
        // Bounded in practice — masters are a small fraction of rows.
        let mut sql = String::from(
            "SELECT account, json FROM calendar_events \
             WHERE (recurring = 1 OR (start_ms < ?1 AND end_ms > ?2))",
        );
        if !accounts.is_empty() {
            sql.push_str(" AND account IN (");
            for i in 0..accounts.len() {
                if i > 0 {
                    sql.push(',');
                }
                // Bound parameters start after the two span placeholders.
                sql.push_str(&format!("?{}", i + 3));
            }
            sql.push(')');
        }
        sql.push_str(" ORDER BY start_ms, uid, rid");

        let mut stmt = self.conn().prepare(&sql)?;
        let mut binds: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(to_ms), Box::new(from_ms)];
        for a in accounts {
            binds.push(Box::new(a.clone()));
        }
        let refs: Vec<&dyn rusqlite::ToSql> = binds.iter().map(|b| b.as_ref()).collect();
        let rows = stmt.query_map(refs.as_slice(), |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;
        Ok(rows.flatten().collect())
    }

    fn put_calendar_events(&self, account: &str, rows: &[CalendarRow]) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let conn = self.conn();
        // One transaction for the batch: a 2000-event account is otherwise 2000
        // WAL commits on the background lane, competing for the write lock the
        // compositor's session persist needs.
        conn.execute_batch("BEGIN")?;
        let res = upsert_calendar_rows(conn, account, rows);
        if res.is_ok() {
            conn.execute_batch("COMMIT")?;
        } else {
            let _ = conn.execute_batch("ROLLBACK"); // best-effort: rollback after a failed statement; the original error is returned as `res`
        }
        res
    }

    fn delete_calendar_events(&self, account: &str, uids: &[String]) -> Result<()> {
        if uids.is_empty() {
            return Ok(());
        }
        let conn = self.conn();
        conn.execute_batch("BEGIN")?;
        let res = delete_calendar_tombstones(conn, account, &CalendarTombstones::Uids(uids));
        if res.is_ok() {
            conn.execute_batch("COMMIT")?;
        } else {
            let _ = conn.execute_batch("ROLLBACK"); // best-effort: rollback after a failed statement; the original error is returned as `res`
        }
        res
    }

    fn delete_calendar_events_by_href(
        &self,
        account: &str,
        resources: &[(String, String)],
    ) -> Result<()> {
        if resources.is_empty() {
            return Ok(());
        }
        let conn = self.conn();
        conn.execute_batch("BEGIN")?;
        let res =
            delete_calendar_tombstones(conn, account, &CalendarTombstones::Resources(resources));
        if res.is_ok() {
            conn.execute_batch("COMMIT")?;
        } else {
            let _ = conn.execute_batch("ROLLBACK"); // best-effort: rollback after a failed statement; the original error is returned as `res`
        }
        res
    }

    fn replace_calendar_account(&self, account: &str, rows: &[CalendarRow]) -> Result<()> {
        let conn = self.conn();
        // Delete-then-insert inside ONE transaction: a crash between the two
        // would otherwise leave the account's calendar empty on disk.
        conn.execute_batch("BEGIN")?;
        let res = replace_calendar_rows(conn, account, rows).map(|_| ());
        if res.is_ok() {
            conn.execute_batch("COMMIT")?;
        } else {
            let _ = conn.execute_batch("ROLLBACK"); // best-effort: rollback after a failed statement; the original error is returned as `res`
        }
        res
    }

    fn get_calendar_sync(&self, account: &str) -> Result<Option<CalendarSyncRow>> {
        Ok(self
            .conn()
            .query_row(
                r#"SELECT account, provider, sync_token, fetched_at, last_error,
                          horizon_from_ms, horizon_to_ms
                   FROM calendar_sync WHERE account = ?1"#,
                params![account],
                |r| {
                    Ok(CalendarSyncRow {
                        account: r.get(0)?,
                        provider: r.get(1)?,
                        sync_token: r.get(2)?,
                        fetched_at: r.get(3)?,
                        last_error: r.get(4)?,
                        horizon_from_ms: r.get(5)?,
                        horizon_to_ms: r.get(6)?,
                    })
                },
            )
            .optional()?)
    }

    fn put_calendar_sync(
        &self,
        account: &str,
        provider: &str,
        sync_token: &str,
        from_ms: i64,
        to_ms: i64,
    ) -> Result<()> {
        write_calendar_cursor(
            self.conn(),
            account,
            &CalendarCursor {
                provider,
                sync_token,
                horizon_from_ms: from_ms,
                horizon_to_ms: to_ms,
            },
        )
    }

    fn set_calendar_error(&self, account: &str, err: &str) -> Result<()> {
        // Deliberately does NOT touch `sync_token` or the events: the prior
        // cache stays valid and the next attempt still resumes incrementally.
        //
        // It DOES advance `fetched_at`, which is the attempt stamp the
        // freshness guard reads — without that, a provider that fails (or keeps
        // returning an empty calendar) would be re-hit every time the popup
        // opens instead of on the normal cadence.
        write_calendar_error(self.conn(), account, err)
    }

    fn has_calendar_events(&self, account: &str) -> Result<bool> {
        let n: i64 = self.conn().query_row(
            "SELECT EXISTS(SELECT 1 FROM calendar_events WHERE account = ?1)",
            params![account],
            |r| r.get(0),
        )?;
        Ok(n != 0)
    }

    fn apply_calendar_page(
        &self,
        account: &str,
        write: &CalendarPageWrite<'_>,
        cursor: &CalendarCursor<'_>,
    ) -> Result<CalendarPageApplied> {
        let tx = Transaction::new_unchecked(self.conn(), TransactionBehavior::Immediate)?;
        let applied = match write {
            CalendarPageWrite::Unchanged => {
                write_calendar_cursor(&tx, account, cursor)?;
                CalendarPageApplied::FreshnessOnly
            }
            CalendarPageWrite::Replace { rows, empty }
                if rows.is_empty() && matches!(empty, EmptySnapshot::KeepCachedEvents) =>
            {
                if calendar_account_has_events(&tx, account)? {
                    write_calendar_error(&tx, account, "provider returned an empty calendar")?;
                    CalendarPageApplied::RejectedEmpty
                } else {
                    let removed = replace_calendar_rows(&tx, account, rows)?;
                    write_calendar_cursor(&tx, account, cursor)?;
                    CalendarPageApplied::Applied {
                        events_changed: removed > 0 || !rows.is_empty(),
                    }
                }
            }
            CalendarPageWrite::Replace { rows, .. } => {
                let removed = replace_calendar_rows(&tx, account, rows)?;
                write_calendar_cursor(&tx, account, cursor)?;
                CalendarPageApplied::Applied {
                    events_changed: removed > 0 || !rows.is_empty(),
                }
            }
            CalendarPageWrite::Delta { rows, tombstones } => {
                upsert_calendar_rows(&tx, account, rows)?;
                delete_calendar_tombstones(&tx, account, tombstones)?;
                write_calendar_cursor(&tx, account, cursor)?;
                CalendarPageApplied::Applied {
                    events_changed: !rows.is_empty() || !tombstones.is_empty(),
                }
            }
        };
        tx.commit()?;
        Ok(applied)
    }

    fn prune_calendar_events(&self, before_ms: i64) -> Result<usize> {
        // Recurrence masters are never pruned by age — an old DTSTART still
        // generates today's occurrences.
        Ok(self.conn().execute(
            "DELETE FROM calendar_events WHERE recurring = 0 AND end_ms < ?1",
            params![before_ms],
        )?)
    }
}

fn write_calendar_cursor(
    conn: &Connection,
    account: &str,
    cursor: &CalendarCursor<'_>,
) -> Result<()> {
    conn.execute(
        r#"INSERT INTO calendar_sync
             (account, provider, sync_token, fetched_at, last_error,
              horizon_from_ms, horizon_to_ms)
           VALUES (?1, ?2, ?3, ?4, '', ?5, ?6)
           ON CONFLICT(account) DO UPDATE SET
             provider = ?2, sync_token = ?3, fetched_at = ?4,
             last_error = '', horizon_from_ms = ?5, horizon_to_ms = ?6"#,
        params![
            account,
            cursor.provider,
            cursor.sync_token,
            util::now(),
            cursor.horizon_from_ms,
            cursor.horizon_to_ms
        ],
    )?;
    Ok(())
}

fn write_calendar_error(conn: &Connection, account: &str, error: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO calendar_sync (account, last_error, fetched_at) VALUES (?1, ?2, ?3) \
         ON CONFLICT(account) DO UPDATE SET last_error = ?2, fetched_at = ?3",
        params![account, error, util::now()],
    )?;
    Ok(())
}

fn calendar_account_has_events(conn: &Connection, account: &str) -> Result<bool> {
    let exists: i64 = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM calendar_events WHERE account = ?1)",
        params![account],
        |row| row.get(0),
    )?;
    Ok(exists != 0)
}

fn upsert_calendar_rows(conn: &Connection, account: &str, rows: &[CalendarRow]) -> Result<()> {
    let hrefs: BTreeSet<&str> = rows.iter().filter_map(|row| row.href.as_deref()).collect();
    let mut delete_resource =
        conn.prepare_cached("DELETE FROM calendar_events WHERE account = ?1 AND href = ?2")?;
    for href in hrefs {
        delete_resource.execute(params![account, href])?;
    }
    let now = util::now();
    let mut stmt = conn.prepare_cached(
        r#"INSERT INTO calendar_events
             (account, uid, rid, calendar, start_ms, end_ms, recurring, json, fetched_at, href)
           VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
           ON CONFLICT(account, uid, rid) DO UPDATE SET
             calendar = ?4, start_ms = ?5, end_ms = ?6,
             recurring = ?7, json = ?8, fetched_at = ?9, href = ?10"#,
    )?;
    for row in rows {
        stmt.execute(params![
            account,
            row.uid,
            row.recurrence_key,
            row.calendar,
            row.start_ms,
            row.end_ms,
            row.recurring as i64,
            row.json,
            now,
            row.href
        ])?;
    }
    Ok(())
}

fn insert_replacement_rows(conn: &Connection, account: &str, rows: &[CalendarRow]) -> Result<()> {
    let now = util::now();
    let mut stmt = conn.prepare_cached(
        r#"INSERT OR REPLACE INTO calendar_events
             (account, uid, rid, calendar, start_ms, end_ms, recurring, json, fetched_at, href)
           VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)"#,
    )?;
    for row in rows {
        stmt.execute(params![
            account,
            row.uid,
            row.recurrence_key,
            row.calendar,
            row.start_ms,
            row.end_ms,
            row.recurring as i64,
            row.json,
            now,
            row.href
        ])?;
    }
    Ok(())
}

fn replace_calendar_rows(conn: &Connection, account: &str, rows: &[CalendarRow]) -> Result<usize> {
    let removed = conn.execute(
        "DELETE FROM calendar_events WHERE account = ?1",
        params![account],
    )?;
    insert_replacement_rows(conn, account, rows)?;
    Ok(removed)
}

fn delete_calendar_tombstones(
    conn: &Connection,
    account: &str,
    tombstones: &CalendarTombstones<'_>,
) -> Result<()> {
    match tombstones {
        CalendarTombstones::Uids(uids) => {
            let mut stmt =
                conn.prepare_cached("DELETE FROM calendar_events WHERE account = ?1 AND uid = ?2")?;
            for uid in *uids {
                stmt.execute(params![account, uid])?;
            }
        }
        CalendarTombstones::Resources(resources) => {
            let mut stmt = conn.prepare_cached(
                "DELETE FROM calendar_events WHERE account = ?1 AND (href = ?2 OR (href IS NULL AND uid = ?3))",
            )?;
            for (href, legacy_uid) in *resources {
                stmt.execute(params![account, href, legacy_uid])?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(uid: &str) -> CalendarRow {
        CalendarRow {
            uid: uid.to_owned(),
            start_ms: 1_000,
            end_ms: 2_000,
            json: format!(r#"{{"uid":"{uid}"}}"#),
            ..Default::default()
        }
    }

    fn cursor(token: &str) -> CalendarCursor<'_> {
        CalendarCursor {
            provider: "test-provider",
            sync_token: token,
            horizon_from_ms: 100,
            horizon_to_ms: 900,
        }
    }

    fn events(db: &Db, account: &str) -> Vec<String> {
        db.get_calendar_events(0, i64::MAX, &[account.to_owned()])
            .unwrap()
            .into_iter()
            .map(|(_, json)| json)
            .collect()
    }

    fn token(db: &Db, account: &str) -> String {
        match db.get_calendar_sync(account).unwrap() {
            Some(sync) => sync.sync_token,
            None => String::new(),
        }
    }

    fn seeded() -> Db {
        let db = Db::open_memory().unwrap();
        let prior = [row("prior")];
        db.replace_calendar_account("acct", &prior).unwrap();
        db.put_calendar_sync("acct", "test-provider", "old-token", 1, 2)
            .unwrap();
        db
    }

    fn reject_cursor_writes(db: &Db) {
        db.conn()
            .execute_batch(
                "CREATE TRIGGER reject_calendar_sync_insert BEFORE INSERT ON calendar_sync \
                 BEGIN SELECT RAISE(ABORT, 'injected insert'); END; \
                 CREATE TRIGGER reject_calendar_sync_update BEFORE UPDATE ON calendar_sync \
                 BEGIN SELECT RAISE(ABORT, 'injected update'); END;",
            )
            .unwrap();
    }

    #[test]
    fn delta_rolls_back_upserts_when_a_tombstone_fails() {
        let db = seeded();
        db.conn()
            .execute_batch(
                "CREATE TRIGGER reject_calendar_delete BEFORE DELETE ON calendar_events \
                 BEGIN SELECT RAISE(ABORT, 'injected'); END;",
            )
            .unwrap();
        let additions = [row("new")];
        let deletions = ["prior".to_owned()];
        let result = db.apply_calendar_page(
            "acct",
            &CalendarPageWrite::Delta {
                rows: &additions,
                tombstones: CalendarTombstones::Uids(&deletions),
            },
            &cursor("new-token"),
        );
        assert!(result.is_err(), "injected tombstone failure must propagate");
        let observed = events(&db, "acct");
        assert_eq!(
            observed,
            vec![r#"{"uid":"prior"}"#.to_owned()],
            "rows: {observed:?}"
        );
        let observed_token = token(&db, "acct");
        assert_eq!(observed_token, "old-token", "token: {observed_token}");
    }

    #[test]
    fn delta_rolls_back_when_a_later_upsert_fails() {
        let db = seeded();
        db.conn()
            .execute_batch(
                "CREATE TRIGGER reject_boom_calendar_insert BEFORE INSERT ON calendar_events \
                 WHEN NEW.uid = 'boom' BEGIN SELECT RAISE(ABORT, 'injected'); END;",
            )
            .unwrap();
        let additions = [row("ok"), row("boom")];
        let result = db.apply_calendar_page(
            "acct",
            &CalendarPageWrite::Delta {
                rows: &additions,
                tombstones: CalendarTombstones::Uids(&[]),
            },
            &cursor("new-token"),
        );
        assert!(result.is_err(), "injected upsert failure must propagate");
        let observed = events(&db, "acct");
        assert_eq!(
            observed,
            vec![r#"{"uid":"prior"}"#.to_owned()],
            "rows: {observed:?}"
        );
        let observed_token = token(&db, "acct");
        assert_eq!(observed_token, "old-token", "token: {observed_token}");
    }

    #[test]
    fn delta_rolls_back_when_the_cursor_write_fails() {
        let db = seeded();
        reject_cursor_writes(&db);
        let additions = [row("new")];
        let deletions = ["prior".to_owned()];
        let result = db.apply_calendar_page(
            "acct",
            &CalendarPageWrite::Delta {
                rows: &additions,
                tombstones: CalendarTombstones::Uids(&deletions),
            },
            &cursor("new-token"),
        );
        assert!(result.is_err(), "injected cursor failure must propagate");
        let observed = events(&db, "acct");
        assert_eq!(
            observed,
            vec![r#"{"uid":"prior"}"#.to_owned()],
            "rows: {observed:?}"
        );
        let observed_token = token(&db, "acct");
        assert_eq!(observed_token, "old-token", "token: {observed_token}");
    }

    #[test]
    fn replace_rolls_back_when_the_replacement_delete_fails() {
        let db = seeded();
        db.conn()
            .execute_batch(
                "CREATE TRIGGER reject_calendar_delete BEFORE DELETE ON calendar_events \
                 BEGIN SELECT RAISE(ABORT, 'injected'); END;",
            )
            .unwrap();
        let replacements = [row("replacement")];
        let result = db.apply_calendar_page(
            "acct",
            &CalendarPageWrite::Replace {
                rows: &replacements,
                empty: EmptySnapshot::Believe,
            },
            &cursor("new-token"),
        );
        assert!(
            result.is_err(),
            "injected replacement delete must propagate"
        );
        let observed = events(&db, "acct");
        assert_eq!(
            observed,
            vec![r#"{"uid":"prior"}"#.to_owned()],
            "rows: {observed:?}"
        );
        let observed_token = token(&db, "acct");
        assert_eq!(observed_token, "old-token", "token: {observed_token}");
    }

    #[test]
    fn replace_rolls_back_when_a_later_insert_fails() {
        let db = seeded();
        db.conn()
            .execute_batch(
                "CREATE TRIGGER reject_boom_calendar_insert BEFORE INSERT ON calendar_events \
                 WHEN NEW.uid = 'boom' BEGIN SELECT RAISE(ABORT, 'injected'); END;",
            )
            .unwrap();
        let replacements = [row("ok"), row("boom")];
        let result = db.apply_calendar_page(
            "acct",
            &CalendarPageWrite::Replace {
                rows: &replacements,
                empty: EmptySnapshot::Believe,
            },
            &cursor("new-token"),
        );
        assert!(result.is_err(), "injected insert failure must propagate");
        let observed = events(&db, "acct");
        assert_eq!(
            observed,
            vec![r#"{"uid":"prior"}"#.to_owned()],
            "rows: {observed:?}"
        );
        let observed_token = token(&db, "acct");
        assert_eq!(observed_token, "old-token", "token: {observed_token}");
    }

    #[test]
    fn replace_rolls_back_when_the_cursor_write_fails() {
        let db = seeded();
        reject_cursor_writes(&db);
        let replacements = [row("replacement")];
        let result = db.apply_calendar_page(
            "acct",
            &CalendarPageWrite::Replace {
                rows: &replacements,
                empty: EmptySnapshot::Believe,
            },
            &cursor("new-token"),
        );
        assert!(result.is_err(), "injected cursor failure must propagate");
        let observed = events(&db, "acct");
        assert_eq!(
            observed,
            vec![r#"{"uid":"prior"}"#.to_owned()],
            "rows: {observed:?}"
        );
        let observed_token = token(&db, "acct");
        assert_eq!(observed_token, "old-token", "token: {observed_token}");
    }

    #[test]
    fn unchanged_rolls_back_and_errors_when_the_cursor_write_fails() {
        let db = seeded();
        reject_cursor_writes(&db);
        let result =
            db.apply_calendar_page("acct", &CalendarPageWrite::Unchanged, &cursor("new-token"));
        assert!(result.is_err(), "injected cursor failure must propagate");
        let observed = events(&db, "acct");
        assert_eq!(
            observed,
            vec![r#"{"uid":"prior"}"#.to_owned()],
            "rows: {observed:?}"
        );
        let observed_token = token(&db, "acct");
        assert_eq!(observed_token, "old-token", "token: {observed_token}");
    }

    #[test]
    fn a_commit_constraint_failure_rolls_back_events_and_cursor() {
        let db = seeded();
        db.conn()
            .execute_batch(
                "PRAGMA foreign_keys = ON; \
                 CREATE TABLE calendar_commit_parent (id INTEGER PRIMARY KEY); \
                 CREATE TABLE calendar_commit_child (parent_id INTEGER NOT NULL, \
                   FOREIGN KEY(parent_id) REFERENCES calendar_commit_parent(id) \
                   DEFERRABLE INITIALLY DEFERRED); \
                 CREATE TRIGGER reject_calendar_commit AFTER INSERT ON calendar_events \
                   BEGIN INSERT INTO calendar_commit_child(parent_id) VALUES (999); END;",
            )
            .unwrap();
        let additions = [row("new")];
        let result = db.apply_calendar_page(
            "acct",
            &CalendarPageWrite::Delta {
                rows: &additions,
                tombstones: CalendarTombstones::Uids(&[]),
            },
            &cursor("new-token"),
        );
        assert!(result.is_err(), "deferred commit constraint must propagate");
        let observed = events(&db, "acct");
        assert_eq!(
            observed,
            vec![r#"{"uid":"prior"}"#.to_owned()],
            "rows: {observed:?}"
        );
        let observed_token = token(&db, "acct");
        assert_eq!(observed_token, "old-token", "token: {observed_token}");
    }

    #[test]
    fn delta_commits_rows_tombstones_and_cursor_together() {
        let db = seeded();
        let additions = [row("new")];
        let deletions = ["prior".to_owned()];
        let result = db
            .apply_calendar_page(
                "acct",
                &CalendarPageWrite::Delta {
                    rows: &additions,
                    tombstones: CalendarTombstones::Uids(&deletions),
                },
                &cursor("new-token"),
            )
            .unwrap();
        assert_eq!(
            result,
            CalendarPageApplied::Applied {
                events_changed: true
            }
        );
        let observed = events(&db, "acct");
        assert_eq!(
            observed,
            vec![r#"{"uid":"new"}"#.to_owned()],
            "rows: {observed:?}"
        );
        let observed_token = token(&db, "acct");
        assert_eq!(observed_token, "new-token", "token: {observed_token}");
    }

    #[test]
    fn unchanged_page_stamps_the_cursor_and_touches_no_event() {
        let db = seeded();
        db.set_calendar_error("acct", "old failure").unwrap();
        let before = events(&db, "acct");
        let result = db
            .apply_calendar_page("acct", &CalendarPageWrite::Unchanged, &cursor("new-token"))
            .unwrap();
        assert_eq!(result, CalendarPageApplied::FreshnessOnly);
        let observed = events(&db, "acct");
        assert_eq!(observed, before, "rows: {observed:?}");
        let sync = db.get_calendar_sync("acct").unwrap().unwrap();
        assert_eq!(sync.sync_token, "new-token", "token: {}", sync.sync_token);
        assert_eq!(sync.provider, "test-provider");
        assert_eq!(sync.horizon_from_ms, 100);
        assert_eq!(sync.horizon_to_ms, 900);
        assert!(
            sync.last_error.is_empty(),
            "last_error: {}",
            sync.last_error
        );
    }

    #[test]
    fn an_empty_snapshot_over_a_cached_account_is_rejected_and_keeps_the_cursor() {
        let db = seeded();
        let result = db
            .apply_calendar_page(
                "acct",
                &CalendarPageWrite::Replace {
                    rows: &[],
                    empty: EmptySnapshot::KeepCachedEvents,
                },
                &cursor("new-token"),
            )
            .unwrap();
        assert_eq!(result, CalendarPageApplied::RejectedEmpty);
        let observed = events(&db, "acct");
        assert_eq!(
            observed,
            vec![r#"{"uid":"prior"}"#.to_owned()],
            "rows: {observed:?}"
        );
        let sync = db.get_calendar_sync("acct").unwrap().unwrap();
        assert_eq!(sync.sync_token, "old-token", "token: {}", sync.sync_token);
        assert_eq!(sync.last_error, "provider returned an empty calendar");
    }

    #[test]
    fn an_empty_snapshot_without_cached_rows_is_accepted() {
        let db = Db::open_memory().unwrap();
        let result = db
            .apply_calendar_page(
                "acct",
                &CalendarPageWrite::Replace {
                    rows: &[],
                    empty: EmptySnapshot::KeepCachedEvents,
                },
                &cursor("empty-token"),
            )
            .unwrap();
        assert_eq!(
            result,
            CalendarPageApplied::Applied {
                events_changed: false
            }
        );
        let observed = events(&db, "acct");
        assert!(observed.is_empty(), "rows: {observed:?}");
        let sync = db.get_calendar_sync("acct").unwrap().unwrap();
        assert_eq!(sync.sync_token, "empty-token", "token: {}", sync.sync_token);
    }

    #[test]
    fn a_reset_replaces_even_when_empty() {
        let db = seeded();
        let result = db
            .apply_calendar_page(
                "acct",
                &CalendarPageWrite::Replace {
                    rows: &[],
                    empty: EmptySnapshot::Believe,
                },
                &cursor("new-token"),
            )
            .unwrap();
        assert_eq!(
            result,
            CalendarPageApplied::Applied {
                events_changed: true
            }
        );
        let observed = events(&db, "acct");
        assert!(observed.is_empty(), "rows: {observed:?}");
        let observed_token = token(&db, "acct");
        assert_eq!(observed_token, "new-token", "token: {observed_token}");
    }

    #[test]
    fn resource_tombstones_delete_every_row_of_the_resource() {
        let db = Db::open_memory().unwrap();
        let source_rows = [
            CalendarRow {
                uid: "resource-a".to_owned(),
                recurrence_key: "rid-a".to_owned(),
                href: Some("/resource".to_owned()),
                ..row("resource-a")
            },
            CalendarRow {
                uid: "resource-b".to_owned(),
                recurrence_key: "rid-b".to_owned(),
                href: Some("/resource".to_owned()),
                ..row("resource-b")
            },
            row("legacy"),
        ];
        db.replace_calendar_account("acct", &source_rows).unwrap();
        let resources = [
            ("/resource".to_owned(), "resource-a".to_owned()),
            ("/legacy-resource".to_owned(), "legacy".to_owned()),
        ];
        let result = db
            .apply_calendar_page(
                "acct",
                &CalendarPageWrite::Delta {
                    rows: &[],
                    tombstones: CalendarTombstones::Resources(&resources),
                },
                &cursor("new-token"),
            )
            .unwrap();
        assert_eq!(
            result,
            CalendarPageApplied::Applied {
                events_changed: true
            }
        );
        let observed = events(&db, "acct");
        assert!(observed.is_empty(), "rows: {observed:?}");
    }
}
