//! AccountStore state — the embedded-SQLite implementation of the [`AccountStore`] seam.
//! Sibling `impl` block (via the `conn()` accessor) so the pinned `db.rs`
//! only carries the schema DDL, not these bodies. The DB is a cache; git /
//! the live source is truth. A server backend implements this trait against
//! Postgres for shared, multi-user state.

use crate::db::Db;
use crate::store::AccountStore;
use anyhow::Result;
use rusqlite::{OptionalExtension, params};

impl AccountStore for Db {
    /// The credential-home dir for a managed account, `None` if unknown.
    fn account_dir(&self, provider: &str, name: &str) -> Result<Option<String>> {
        let r = self
            .conn()
            .query_row(
                "SELECT dir FROM accounts WHERE provider=?1 AND name=?2",
                params![provider, name],
                |r| r.get::<_, String>(0),
            )
            .optional()?;
        Ok(r)
    }

    /// Every managed account for a provider, as `(name, dir, managed)`.
    fn list_accounts(&self, provider: &str) -> Result<Vec<(String, String, bool)>> {
        let mut stmt = self.conn().prepare(
            "SELECT name, dir, managed FROM accounts WHERE provider=?1 \
             ORDER BY last_used DESC, name",
        )?;
        let rows = stmt
            .query_map(params![provider], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)? != 0,
                ))
            })?
            .filter_map(|r| r.ok())
            .collect();
        Ok(rows)
    }

    /// Register (or update) an account's credential-home dir.
    fn put_account(
        &self,
        provider: &str,
        name: &str,
        dir: &str,
        managed: bool,
        now_ms: i64,
    ) -> Result<()> {
        self.conn().execute(
            "INSERT INTO accounts (provider, name, dir, managed, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(provider, name) DO UPDATE SET dir=?3, managed=?4",
            params![provider, name, dir, managed as i64, now_ms],
        )?;
        Ok(())
    }

    /// Mark an account as just used (for picker ordering).
    fn touch_account(&self, provider: &str, name: &str, now_ms: i64) -> Result<()> {
        self.conn().execute(
            "UPDATE accounts SET last_used=?3 WHERE provider=?1 AND name=?2",
            params![provider, name, now_ms],
        )?;
        Ok(())
    }

    /// Forget a managed account (does not delete its on-disk credential dir).
    fn del_account(&self, provider: &str, name: &str) -> Result<()> {
        self.conn().execute(
            "DELETE FROM accounts WHERE provider=?1 AND name=?2",
            params![provider, name],
        )?;
        Ok(())
    }
}
