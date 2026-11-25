//! Remote sandbox health state (schema v118): the sampler's latest and
//! previous probe reading per sandbox, and the bounded event journal
//! (OpenSpec `remote-sandbox-visibility`). Sibling `impl` block (via the
//! `conn()` accessor) so `db.rs` only carries the schema bump. The DB is a
//! cache: a lost row only costs one sample of restart-detection context.

use anyhow::Result;
use rusqlite::{OptionalExtension, params};

use crate::db::Db;
use crate::remote_health::HealthSample;

/// Journal rows kept per sandbox.
pub const EVENTS_PER_SANDBOX: i64 = 500;

/// Journal rows older than this are dropped (30 days).
pub const EVENT_MAX_AGE_MS: i64 = 30 * 24 * 3_600 * 1_000;

/// The stored reading of one sandbox: the latest sample and the one before.
#[derive(Debug, Clone, PartialEq)]
pub struct RemoteHealthRow {
    pub sampled_at_ms: i64,
    pub sample: Option<HealthSample>,
    pub prev_sample: Option<HealthSample>,
    /// The provider's state word at the time of the sample.
    pub provider_state: Option<String>,
}

/// One journal row (`kind` is `restart`, `oom_kill`, `lifetime`, …).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteEventRow {
    pub id: i64,
    pub at_ms: i64,
    pub kind: String,
    /// Compact JSON detail.
    pub detail: String,
}

fn parse_sample(text: Option<String>) -> Option<HealthSample> {
    text.and_then(|t| serde_json::from_str(&t).ok())
}

impl Db {
    /// The stored reading for `key`, if any. An unparsable sample reads back
    /// as `None` (a schema drift costs one sample, never an error).
    pub fn remote_health(&self, key: &str) -> Result<Option<RemoteHealthRow>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT sampled_at_ms, sample, prev_sample, provider_state
                   FROM remote_health WHERE sandbox_key=?1",
                [key],
                |r| {
                    Ok(RemoteHealthRow {
                        sampled_at_ms: r.get(0)?,
                        sample: parse_sample(r.get(1)?),
                        prev_sample: parse_sample(r.get(2)?),
                        provider_state: r.get(3)?,
                    })
                },
            )
            .optional()?)
    }

    /// Record a new sample for `key`; the stored latest becomes the previous.
    pub fn put_remote_health(
        &self,
        key: &str,
        now_ms: i64,
        sample: &HealthSample,
        provider_state: Option<&str>,
    ) -> Result<()> {
        let json = serde_json::to_string(sample)?;
        self.conn().execute(
            "INSERT INTO remote_health(sandbox_key, sampled_at_ms, sample, prev_sample, provider_state)
             VALUES(?1, ?2, ?3, NULL, ?4)
             ON CONFLICT(sandbox_key) DO UPDATE SET
               prev_sample=remote_health.sample,
               sample=excluded.sample,
               sampled_at_ms=excluded.sampled_at_ms,
               provider_state=excluded.provider_state",
            params![key, now_ms, json, provider_state],
        )?;
        Ok(())
    }

    /// Append a journal row, then trim `key`'s journal to
    /// [`EVENTS_PER_SANDBOX`] rows no older than [`EVENT_MAX_AGE_MS`].
    pub fn add_remote_event(&self, key: &str, at_ms: i64, kind: &str, detail: &str) -> Result<()> {
        let tx = self.conn().unchecked_transaction()?;
        tx.execute(
            "INSERT INTO remote_events(sandbox_key, at_ms, kind, detail) VALUES(?1,?2,?3,?4)",
            params![key, at_ms, kind, detail],
        )?;
        tx.execute(
            "DELETE FROM remote_events WHERE sandbox_key=?1 AND (at_ms < ?2 OR id NOT IN (
               SELECT id FROM remote_events WHERE sandbox_key=?1 ORDER BY id DESC LIMIT ?3))",
            params![
                key,
                at_ms.saturating_sub(EVENT_MAX_AGE_MS),
                EVENTS_PER_SANDBOX
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// The newest `limit` journal rows for `key`, newest first.
    pub fn remote_events(&self, key: &str, limit: usize) -> Result<Vec<RemoteEventRow>> {
        let mut stmt = self.conn().prepare(
            "SELECT id, at_ms, kind, detail FROM remote_events
              WHERE sandbox_key=?1 ORDER BY id DESC LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(params![key, limit as i64], |r| {
                Ok(RemoteEventRow {
                    id: r.get(0)?,
                    at_ms: r.get(1)?,
                    kind: r.get(2)?,
                    detail: r.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(boot: &str) -> HealthSample {
        HealthSample {
            boot_id: Some(boot.into()),
            ..HealthSample::default()
        }
    }

    #[test]
    fn a_new_sample_pushes_the_latest_to_previous() {
        let db = Db::open_memory().unwrap();
        assert_eq!(db.remote_health("e/s").unwrap(), None);
        db.put_remote_health("e/s", 10, &sample("a"), Some("running"))
            .unwrap();
        let first = db.remote_health("e/s").unwrap().unwrap();
        assert_eq!(first.sample, Some(sample("a")));
        assert_eq!(first.prev_sample, None);
        db.put_remote_health("e/s", 20, &sample("b"), None).unwrap();
        let second = db.remote_health("e/s").unwrap().unwrap();
        assert_eq!(second.sampled_at_ms, 20);
        assert_eq!(second.sample, Some(sample("b")));
        assert_eq!(second.prev_sample, Some(sample("a")));
        assert_eq!(second.provider_state, None);
        assert_eq!(db.remote_health("other").unwrap(), None);
    }

    #[test]
    fn the_journal_is_bounded_per_sandbox_and_by_age() {
        let db = Db::open_memory().unwrap();
        let now = 10 * EVENT_MAX_AGE_MS;
        db.add_remote_event("e/old", now - EVENT_MAX_AGE_MS - 1, "restart", "{}")
            .unwrap();
        db.add_remote_event("e/old", now, "oom_kill", "{}").unwrap();
        let kept = db.remote_events("e/old", 10).unwrap();
        assert_eq!(kept.len(), 1, "the 30-day-old row is trimmed");
        assert_eq!(kept[0].kind, "oom_kill");

        for i in 0..(EVENTS_PER_SANDBOX + 5) {
            db.add_remote_event("e/busy", now + i, "restart", &format!("{{\"n\":{i}}}"))
                .unwrap();
        }
        db.add_remote_event("e/quiet", now, "restart", "{}")
            .unwrap();
        let busy = db.remote_events("e/busy", 10_000).unwrap();
        assert_eq!(busy.len() as i64, EVENTS_PER_SANDBOX);
        assert_eq!(
            busy[0].detail,
            format!("{{\"n\":{}}}", EVENTS_PER_SANDBOX + 4)
        );
        assert_eq!(db.remote_events("e/quiet", 10).unwrap().len(), 1);
    }
}
