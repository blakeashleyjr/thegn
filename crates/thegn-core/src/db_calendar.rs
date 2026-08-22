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

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};

use crate::db::Db;
use crate::store::{CalendarRow, CalendarStore, CalendarSyncRow};
use crate::util;

/// Create the calendar cache tables. Idempotent, so re-running is a no-op.
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
        sql.push_str(" ORDER BY start_ms");

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
        let res = (|| -> Result<()> {
            let now = util::now();
            let mut stmt = conn.prepare_cached(
                r#"INSERT INTO calendar_events
                     (account, uid, calendar, start_ms, end_ms, recurring, json, fetched_at)
                   VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                   ON CONFLICT(account, uid) DO UPDATE SET
                     calendar = ?3, start_ms = ?4, end_ms = ?5,
                     recurring = ?6, json = ?7, fetched_at = ?8"#,
            )?;
            for r in rows {
                stmt.execute(params![
                    account,
                    r.uid,
                    r.calendar,
                    r.start_ms,
                    r.end_ms,
                    r.recurring as i64,
                    r.json,
                    now
                ])?;
            }
            Ok(())
        })();
        if res.is_ok() {
            conn.execute_batch("COMMIT")?;
        } else {
            let _ = conn.execute_batch("ROLLBACK");
        }
        res
    }

    fn delete_calendar_events(&self, account: &str, uids: &[String]) -> Result<()> {
        if uids.is_empty() {
            return Ok(());
        }
        let conn = self.conn();
        conn.execute_batch("BEGIN")?;
        let res = (|| -> Result<()> {
            let mut stmt =
                conn.prepare_cached("DELETE FROM calendar_events WHERE account = ?1 AND uid = ?2")?;
            for uid in uids {
                stmt.execute(params![account, uid])?;
            }
            Ok(())
        })();
        if res.is_ok() {
            conn.execute_batch("COMMIT")?;
        } else {
            let _ = conn.execute_batch("ROLLBACK");
        }
        res
    }

    fn replace_calendar_account(&self, account: &str, rows: &[CalendarRow]) -> Result<()> {
        let conn = self.conn();
        // Delete-then-insert inside ONE transaction: a crash between the two
        // would otherwise leave the account's calendar empty on disk.
        conn.execute_batch("BEGIN")?;
        let res = (|| -> Result<()> {
            conn.execute(
                "DELETE FROM calendar_events WHERE account = ?1",
                params![account],
            )?;
            let now = util::now();
            let mut stmt = conn.prepare_cached(
                r#"INSERT OR REPLACE INTO calendar_events
                     (account, uid, calendar, start_ms, end_ms, recurring, json, fetched_at)
                   VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)"#,
            )?;
            for r in rows {
                stmt.execute(params![
                    account,
                    r.uid,
                    r.calendar,
                    r.start_ms,
                    r.end_ms,
                    r.recurring as i64,
                    r.json,
                    now
                ])?;
            }
            Ok(())
        })();
        if res.is_ok() {
            conn.execute_batch("COMMIT")?;
        } else {
            let _ = conn.execute_batch("ROLLBACK");
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
        self.conn().execute(
            r#"INSERT INTO calendar_sync
                 (account, provider, sync_token, fetched_at, last_error,
                  horizon_from_ms, horizon_to_ms)
               VALUES (?1, ?2, ?3, ?4, '', ?5, ?6)
               ON CONFLICT(account) DO UPDATE SET
                 provider = ?2, sync_token = ?3, fetched_at = ?4,
                 -- A success clears the previous error; otherwise a one-off
                 -- failure would be reported forever.
                 last_error = '',
                 horizon_from_ms = ?5, horizon_to_ms = ?6"#,
            params![account, provider, sync_token, util::now(), from_ms, to_ms],
        )?;
        Ok(())
    }

    fn set_calendar_error(&self, account: &str, err: &str) -> Result<()> {
        // Deliberately does NOT touch `sync_token` or the events: the prior
        // cache stays valid and the next attempt still resumes incrementally.
        //
        // It DOES advance `fetched_at`, which is the attempt stamp the
        // freshness guard reads — without that, a provider that fails (or keeps
        // returning an empty calendar) would be re-hit every time the popup
        // opens instead of on the normal cadence.
        self.conn().execute(
            r#"INSERT INTO calendar_sync (account, last_error, fetched_at)
               VALUES (?1, ?2, ?3)
               ON CONFLICT(account) DO UPDATE SET last_error = ?2, fetched_at = ?3"#,
            params![account, err, util::now()],
        )?;
        Ok(())
    }

    fn has_calendar_events(&self, account: &str) -> Result<bool> {
        let n: i64 = self.conn().query_row(
            "SELECT EXISTS(SELECT 1 FROM calendar_events WHERE account = ?1)",
            params![account],
            |r| r.get(0),
        )?;
        Ok(n != 0)
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
