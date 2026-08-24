//! UsageStore state — the embedded-SQLite implementation of the [`UsageStore`]
//! seam. Sibling `impl` block (via the `conn()` accessor) so the pinned `db.rs`
//! only carries the schema DDL, not these bodies.
//!
//! Every row here is regenerable: the provider is the source of truth and the
//! next poll refills it, so this table is always safe to drop.

use crate::db::Db;
use crate::store::{UsageSample, UsageStore};
use anyhow::Result;
use rusqlite::params;

impl UsageStore for Db {
    fn put_usage_samples(&self, samples: &[UsageSample]) -> Result<()> {
        if samples.is_empty() {
            return Ok(());
        }
        // One transaction per poll: eight accounts × four windows is 32 inserts,
        // and 32 separate WAL commits on the poll path is 32 fsyncs for data
        // that is a cache.
        let conn = self.conn();
        conn.execute_batch("BEGIN")?;
        let result = (|| -> Result<()> {
            let mut stmt = conn.prepare(
                "INSERT INTO usage_samples \
                 (account_key, window, used_percent, resets_at, sampled_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            for s in samples {
                stmt.execute(params![
                    s.account_key,
                    s.window,
                    f64::from(s.used_percent),
                    s.resets_at,
                    s.sampled_at,
                ])?;
            }
            Ok(())
        })();
        match result {
            Ok(()) => {
                conn.execute_batch("COMMIT")?;
                Ok(())
            }
            Err(e) => {
                // best-effort: the rollback of a cache write; the error the
                // caller sees is the insert failure, not this.
                let _ = conn.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    }

    fn usage_history(
        &self,
        account_key: &str,
        window: &str,
        since: i64,
    ) -> Result<Vec<UsageSample>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT used_percent, resets_at, sampled_at FROM usage_samples \
             WHERE account_key=?1 AND window=?2 AND sampled_at>=?3 \
             ORDER BY sampled_at",
        )?;
        let rows = stmt
            .query_map(params![account_key, window, since], |r| {
                Ok(UsageSample {
                    account_key: account_key.to_string(),
                    window: window.to_string(),
                    used_percent: r.get::<_, f64>(0)? as f32,
                    resets_at: r.get::<_, Option<i64>>(1)?,
                    sampled_at: r.get::<_, i64>(2)?,
                })
            })?
            .filter_map(|r| r.ok())
            .collect();
        Ok(rows)
    }

    fn prune_usage_samples(&self, before: i64) -> Result<usize> {
        let n = self.conn().execute(
            "DELETE FROM usage_samples WHERE sampled_at < ?1",
            params![before],
        )?;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::UsageSample;

    fn sample(key: &str, window: &str, pct: f32, at: i64) -> UsageSample {
        UsageSample {
            account_key: key.into(),
            window: window.into(),
            used_percent: pct,
            resets_at: Some(at + 3600),
            sampled_at: at,
        }
    }

    #[test]
    fn samples_round_trip_scoped_and_ordered() {
        let db = Db::open_memory().unwrap();
        db.put_usage_samples(&[
            sample("a", "5h", 30.0, 300),
            sample("a", "5h", 10.0, 100),
            sample("a", "7d", 90.0, 200),
            sample("b", "5h", 50.0, 200),
        ])
        .unwrap();

        let hist = db.usage_history("a", "5h", 0).unwrap();
        // Scoped to the account AND the window, oldest first — the sparkline
        // draws left-to-right in time.
        assert_eq!(hist.len(), 2);
        assert_eq!(hist[0].used_percent, 10.0);
        assert_eq!(hist[1].used_percent, 30.0);
        assert_eq!(hist[0].resets_at, Some(100 + 3600));

        // `since` clips the window.
        assert_eq!(db.usage_history("a", "5h", 200).unwrap().len(), 1);
        // An unknown account/window is empty, not an error.
        assert!(db.usage_history("nope", "5h", 0).unwrap().is_empty());
        assert!(db.usage_history("a", "nope", 0).unwrap().is_empty());
    }

    #[test]
    fn empty_batch_is_a_no_op() {
        let db = Db::open_memory().unwrap();
        db.put_usage_samples(&[]).unwrap();
        assert!(db.usage_history("a", "5h", 0).unwrap().is_empty());
    }

    #[test]
    fn prune_drops_only_what_is_older_than_the_cutoff() {
        let db = Db::open_memory().unwrap();
        db.put_usage_samples(&[
            sample("a", "5h", 1.0, 100),
            sample("a", "5h", 2.0, 200),
            sample("a", "5h", 3.0, 300),
        ])
        .unwrap();
        assert_eq!(db.prune_usage_samples(250).unwrap(), 2);
        let left = db.usage_history("a", "5h", 0).unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].used_percent, 3.0);
        // Pruning again removes nothing rather than erroring.
        assert_eq!(db.prune_usage_samples(250).unwrap(), 0);
    }
}
