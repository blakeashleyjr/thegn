//! SQLite implementation of the Lead-authored automation rule seam
//! (schema v111, THE-2181).

use anyhow::Result;
use rusqlite::{OptionalExtension, params};

use crate::automation_authoring::LEAD_RULES_MAX;
use crate::db::Db;
use crate::store::{AutomationRuleStore, LeadRuleLimitReached, LeadRuleRow};

impl AutomationRuleStore for Db {
    fn put_lead_rule(&self, id: &str, body_json: &str, author: &str, now: i64) -> Result<bool> {
        let conn = self.conn();
        conn.execute_batch("BEGIN IMMEDIATE")?;
        let result = (|| -> Result<bool> {
            let exists = conn
                .query_row(
                    "SELECT 1 FROM automation_rules WHERE id=?1",
                    params![id],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
            if exists {
                conn.execute(
                    "UPDATE automation_rules SET body_json=?2, author=?3, updated_at=?4 \
                     WHERE id=?1",
                    params![id, body_json, author, now],
                )?;
                return Ok(false);
            }
            let count: i64 =
                conn.query_row("SELECT COUNT(*) FROM automation_rules", [], |row| {
                    row.get(0)
                })?;
            if usize::try_from(count).unwrap_or(usize::MAX) >= LEAD_RULES_MAX {
                return Err(LeadRuleLimitReached.into());
            }
            conn.execute(
                "INSERT INTO automation_rules \
                   (id, origin, body_json, enabled, author, created_at, updated_at) \
                 VALUES (?1, 'lead', ?2, 1, ?3, ?4, ?4)",
                params![id, body_json, author, now],
            )?;
            Ok(true)
        })();
        match result {
            Ok(created) => {
                conn.execute_batch("COMMIT")?;
                Ok(created)
            }
            Err(error) => match conn.execute_batch("ROLLBACK") {
                Ok(()) => Err(error),
                Err(rollback) => Err(error.context(format!("rollback failed: {rollback}"))),
            },
        }
    }

    fn delete_lead_rule(&self, id: &str) -> Result<bool> {
        Ok(self
            .conn()
            .execute("DELETE FROM automation_rules WHERE id=?1", params![id])?
            > 0)
    }

    fn set_lead_rule_enabled(&self, id: &str, enabled: bool) -> Result<bool> {
        Ok(self.conn().execute(
            "UPDATE automation_rules SET enabled=?2 WHERE id=?1",
            params![id, i64::from(enabled)],
        )? > 0)
    }

    fn lead_rules(&self) -> Result<Vec<LeadRuleRow>> {
        let mut stmt = self.conn().prepare(
            "SELECT id, body_json, enabled, author, created_at, updated_at \
             FROM automation_rules WHERE origin='lead' \
             ORDER BY created_at, id LIMIT ?1",
        )?;
        let limit = i64::try_from(LEAD_RULES_MAX).unwrap_or(i64::MAX);
        let rows = stmt
            .query_map(params![limit], |row| {
                Ok(LeadRuleRow {
                    id: row.get(0)?,
                    body_json: row.get(1)?,
                    enabled: row.get::<_, i64>(2)? != 0,
                    author: row.get(3)?,
                    created_at: row.get(4)?,
                    updated_at: row.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_creates_then_replaces_keeping_enabled_and_created_at() {
        let db = Db::open_memory().unwrap();
        assert!(db.put_lead_rule("lead:a", "{\"v\":1}", "lead", 10).unwrap());
        assert!(db.set_lead_rule_enabled("lead:a", false).unwrap());
        assert!(!db.put_lead_rule("lead:a", "{\"v\":2}", "lead", 20).unwrap());
        let rows = db.lead_rules().unwrap();
        assert_eq!(
            rows,
            vec![LeadRuleRow {
                id: "lead:a".into(),
                body_json: "{\"v\":2}".into(),
                enabled: false,
                author: "lead".into(),
                created_at: 10,
                updated_at: 20,
            }]
        );
    }

    #[test]
    fn delete_and_enable_report_whether_the_row_existed() {
        let db = Db::open_memory().unwrap();
        assert!(!db.delete_lead_rule("lead:missing").unwrap());
        assert!(!db.set_lead_rule_enabled("lead:missing", true).unwrap());
        db.put_lead_rule("lead:a", "{}", "lead", 1).unwrap();
        db.put_lead_rule("lead:b", "{}", "lead", 2).unwrap();
        assert!(db.set_lead_rule_enabled("lead:b", true).unwrap());
        assert!(db.delete_lead_rule("lead:a").unwrap());
        assert!(!db.delete_lead_rule("lead:a").unwrap());
        let ids: Vec<String> = db.lead_rules().unwrap().into_iter().map(|r| r.id).collect();
        assert_eq!(ids, ["lead:b"]);
    }

    #[test]
    fn creation_is_bounded_but_replacement_is_not() {
        let db = Db::open_memory().unwrap();
        for index in 0..LEAD_RULES_MAX {
            db.put_lead_rule(&format!("lead:r{index}"), "{}", "lead", 1)
                .unwrap();
        }
        let error = db.put_lead_rule("lead:over", "{}", "lead", 2).unwrap_err();
        assert!(error.downcast_ref::<LeadRuleLimitReached>().is_some());
        assert!(!db.put_lead_rule("lead:r0", "{}", "lead", 3).unwrap());
        assert_eq!(db.lead_rules().unwrap().len(), LEAD_RULES_MAX);
        // The failed insert rolled back cleanly: the connection still writes.
        assert!(db.delete_lead_rule("lead:r1").unwrap());
        assert!(db.put_lead_rule("lead:over", "{}", "lead", 4).unwrap());
    }
}
