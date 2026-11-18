//! Per-account inbox state (schema v116, THE-2214): the cursor, the bounded
//! seen-set that decides "is this item new", the baseline flag, and the last
//! poll's outcome.
//!
//! The seen-set lives here, not in the notifications table: read rows are
//! pruned there, and an old item would fire again. Only the daemon's inbox
//! producer writes these rows; it prunes accounts no longer configured
//! ([`Db::prune_inbox_states`]), so there is no vacuum entry.

use anyhow::Result;
use rusqlite::{OptionalExtension, named_params};

use crate::db::Db;
use crate::inbox::{MAX_SEEN, sanitize_text};

/// The longest `last_error` stored, in characters.
pub const MAX_LAST_ERROR_CHARS: usize = 200;

/// One `inbox_account_state` row.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct InboxAccountState {
    pub account: String,
    /// The account's kind (`InboxKind::as_str`).
    pub kind: String,
    /// `InboxAccount::config_digest` at the last successful poll.
    pub config_digest: String,
    pub cursor: String,
    /// Item ids seen, oldest first. Bounded at [`MAX_SEEN`] (newest kept).
    pub seen: Vec<String>,
    /// The first successful poll (the baseline) has happened.
    pub baselined: bool,
    pub last_poll_ms: i64,
    pub last_ok_ms: i64,
    /// Scrubbed and capped at [`MAX_LAST_ERROR_CHARS`]; empty after a success.
    pub last_error: String,
    pub item_count: i64,
}

const COLS: &str = "account, kind, config_digest, cursor, seen_json, baselined, \
                    last_poll_ms, last_ok_ms, last_error, item_count";

fn map_state(row: &rusqlite::Row<'_>) -> rusqlite::Result<InboxAccountState> {
    let seen_json: String = row.get(4)?;
    Ok(InboxAccountState {
        account: row.get(0)?,
        kind: row.get(1)?,
        config_digest: row.get(2)?,
        cursor: row.get(3)?,
        // A corrupt seen-set reads as empty: the next poll re-baselines
        // (surfaces at most FIRST_POLL_SURFACE rows, fires no rule events).
        seen: serde_json::from_str(&seen_json).unwrap_or_default(),
        baselined: row.get::<_, i64>(5)? != 0,
        last_poll_ms: row.get(6)?,
        last_ok_ms: row.get(7)?,
        last_error: row.get(8)?,
        item_count: row.get(9)?,
    })
}

impl Db {
    /// `account`'s state row, if any.
    pub fn inbox_state(&self, account: &str) -> Result<Option<InboxAccountState>> {
        Ok(self
            .conn()
            .query_row(
                &format!("SELECT {COLS} FROM inbox_account_state WHERE account = ?1"),
                [account],
                map_state,
            )
            .optional()?)
    }

    /// Insert or replace `s.account`'s row. The seen-set is truncated to the
    /// newest [`MAX_SEEN`] ids and `last_error` is scrubbed and capped.
    pub fn put_inbox_state(&self, s: &InboxAccountState) -> Result<()> {
        let skip = s.seen.len().saturating_sub(MAX_SEEN);
        let seen_json = serde_json::to_string(&s.seen[skip..])?;
        let last_error = sanitize_text(&s.last_error, MAX_LAST_ERROR_CHARS);
        self.conn().execute(
            "INSERT INTO inbox_account_state
               (account, kind, config_digest, cursor, seen_json, baselined,
                last_poll_ms, last_ok_ms, last_error, item_count)
             VALUES (:account, :kind, :digest, :cursor, :seen, :baselined,
                     :poll, :ok, :error, :count)
             ON CONFLICT(account) DO UPDATE SET
               kind = excluded.kind,
               config_digest = excluded.config_digest,
               cursor = excluded.cursor,
               seen_json = excluded.seen_json,
               baselined = excluded.baselined,
               last_poll_ms = excluded.last_poll_ms,
               last_ok_ms = excluded.last_ok_ms,
               last_error = excluded.last_error,
               item_count = excluded.item_count",
            named_params! {
                ":account": s.account,
                ":kind": s.kind,
                ":digest": s.config_digest,
                ":cursor": s.cursor,
                ":seen": seen_json,
                ":baselined": i64::from(s.baselined),
                ":poll": s.last_poll_ms,
                ":ok": s.last_ok_ms,
                ":error": last_error,
                ":count": s.item_count,
            },
        )?;
        Ok(())
    }

    /// Every state row, ordered by account.
    pub fn inbox_states(&self) -> Result<Vec<InboxAccountState>> {
        let mut stmt = self.conn().prepare(&format!(
            "SELECT {COLS} FROM inbox_account_state ORDER BY account"
        ))?;
        let rows = stmt.query_map([], map_state)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Delete every row whose account is not in `keep`. Returns the number
    /// deleted.
    pub fn prune_inbox_states(&self, keep: &[&str]) -> Result<usize> {
        let tx = self.conn().unchecked_transaction()?;
        let mut stmt = tx.prepare("SELECT account FROM inbox_account_state")?;
        let accounts = stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<String>>>()?;
        drop(stmt);
        let mut removed = 0;
        for account in accounts.iter().filter(|a| !keep.contains(&a.as_str())) {
            removed += tx.execute(
                "DELETE FROM inbox_account_state WHERE account = ?1",
                [account],
            )?;
        }
        tx.commit()?;
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(account: &str) -> InboxAccountState {
        InboxAccountState {
            account: account.into(),
            kind: "command".into(),
            config_digest: "d1".into(),
            cursor: "c1".into(),
            seen: vec!["a".into(), "b".into()],
            baselined: true,
            last_poll_ms: 10,
            last_ok_ms: 9,
            last_error: String::new(),
            item_count: 2,
        }
    }

    #[test]
    fn put_get_round_trips_and_upserts() {
        let db = Db::open_memory().unwrap();
        assert_eq!(db.inbox_state("work").unwrap(), None);
        let s = state("work");
        db.put_inbox_state(&s).unwrap();
        assert_eq!(db.inbox_state("work").unwrap(), Some(s.clone()));
        let next = InboxAccountState {
            cursor: "c2".into(),
            seen: vec!["c".into()],
            baselined: false,
            last_error: "boom".into(),
            item_count: 1,
            ..s
        };
        db.put_inbox_state(&next).unwrap();
        assert_eq!(db.inbox_state("work").unwrap(), Some(next));
        assert_eq!(db.inbox_states().unwrap().len(), 1);
    }

    #[test]
    fn seen_is_bounded_newest_kept() {
        let db = Db::open_memory().unwrap();
        let seen: Vec<String> = (0..MAX_SEEN + 5).map(|i| format!("id{i}")).collect();
        db.put_inbox_state(&InboxAccountState {
            seen,
            ..state("work")
        })
        .unwrap();
        let got = db.inbox_state("work").unwrap().unwrap().seen;
        assert_eq!(got.len(), MAX_SEEN);
        assert_eq!(got.first().map(String::as_str), Some("id5"));
        assert_eq!(got.last().cloned(), Some(format!("id{}", MAX_SEEN + 4)));
    }

    #[test]
    fn last_error_is_scrubbed_and_capped() {
        let db = Db::open_memory().unwrap();
        db.put_inbox_state(&InboxAccountState {
            last_error: format!("bad\u{1b}[31m\u{202E}\n{}", "e".repeat(500)),
            ..state("work")
        })
        .unwrap();
        let err = db.inbox_state("work").unwrap().unwrap().last_error;
        assert_eq!(err.chars().count(), MAX_LAST_ERROR_CHARS);
        assert!(err.starts_with("bad[31m e"), "{err}");
        assert!(!err.chars().any(|c| c.is_control()));
    }

    #[test]
    fn a_corrupt_seen_set_reads_as_empty() {
        let db = Db::open_memory().unwrap();
        db.put_inbox_state(&state("work")).unwrap();
        db.conn()
            .execute("UPDATE inbox_account_state SET seen_json = 'nope'", [])
            .unwrap();
        assert!(db.inbox_state("work").unwrap().unwrap().seen.is_empty());
    }

    #[test]
    fn states_are_ordered_and_prune_keeps_the_configured() {
        let db = Db::open_memory().unwrap();
        for account in ["zeta", "alpha", "mid"] {
            db.put_inbox_state(&state(account)).unwrap();
        }
        let names: Vec<_> = db
            .inbox_states()
            .unwrap()
            .into_iter()
            .map(|s| s.account)
            .collect();
        assert_eq!(names, ["alpha", "mid", "zeta"]);
        assert_eq!(db.prune_inbox_states(&["mid", "gone"]).unwrap(), 2);
        let names: Vec<_> = db
            .inbox_states()
            .unwrap()
            .into_iter()
            .map(|s| s.account)
            .collect();
        assert_eq!(names, ["mid"]);
        assert_eq!(db.prune_inbox_states(&["mid"]).unwrap(), 0);
        assert_eq!(db.prune_inbox_states(&[]).unwrap(), 1);
        assert!(db.inbox_states().unwrap().is_empty());
    }
}
