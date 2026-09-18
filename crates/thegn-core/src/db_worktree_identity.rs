//! Exact worktree identity ledger access.
//!
//! The ledger is intentionally separate from the legacy path/tab registry.
//! Schema creation does not inspect Git and this module never treats a display
//! label or a lossy path as an authority key. Admission callers must first
//! inspect Git, then write a verified row or explicitly quarantine it.

use crate::db::Db;
use crate::models::WorktreeInstanceRow;
use anyhow::{Result, bail};
use rusqlite::{OptionalExtension, params};

const INSTANCE_ID_BYTES: usize = 32;
const GENERATION_BYTES: usize = 16;
const REPOSITORY_ID_BYTES: usize = 32;
const MAX_IDENTITY_FIELD_BYTES: usize = 16 * 1024;

fn validate_row(row: &WorktreeInstanceRow) -> Result<()> {
    for (name, value, exact) in [
        (
            "instance_id",
            row.instance_id.as_slice(),
            Some(INSTANCE_ID_BYTES),
        ),
        (
            "generation",
            row.generation.as_slice(),
            Some(GENERATION_BYTES),
        ),
        ("repo_id", row.repo_id.as_slice(), Some(REPOSITORY_ID_BYTES)),
        ("common_dir", row.common_dir.as_slice(), None),
        ("admin_id", row.admin_id.as_slice(), None),
        ("branch_ref", row.branch_ref.as_slice(), None),
        ("path", row.path.as_slice(), None),
        ("owner", row.owner.as_slice(), None),
    ] {
        if value.is_empty() || value.len() > MAX_IDENTITY_FIELD_BYTES {
            bail!("{name} is empty or exceeds the identity bound");
        }
        if let Some(exact) = exact
            && value.len() != exact
        {
            bail!("{name} must contain exactly {exact} bytes");
        }
    }
    match row.state.as_str() {
        "verified" | "legacy" | "quarantined" | "split" => Ok(()),
        other => bail!("invalid worktree identity state {other:?}"),
    }
}

fn decode(row: &rusqlite::Row<'_>) -> rusqlite::Result<WorktreeInstanceRow> {
    Ok(WorktreeInstanceRow {
        instance_id: row.get(0)?,
        generation: row.get(1)?,
        repo_id: row.get(2)?,
        common_dir: row.get(3)?,
        admin_id: row.get(4)?,
        branch_ref: row.get(5)?,
        path: row.get(6)?,
        owner: row.get(7)?,
        state: row.get(8)?,
        quarantine_reason: row.get(9)?,
        created_at: row.get(10)?,
    })
}

const COLUMNS: &str = "instance_id, generation, repo_id, common_dir, admin_id, branch_ref, path, owner, state, quarantine_reason, created_at";

impl Db {
    /// Insert a verified or explicitly quarantined identity claim. This method
    /// does not inspect Git; callers own that admission step.
    pub fn put_worktree_instance(&self, row: &WorktreeInstanceRow) -> Result<()> {
        validate_row(row)?;
        self.conn().execute(
            "INSERT INTO worktree_instances
               (instance_id, generation, repo_id, common_dir, admin_id, branch_ref, path, owner, state, quarantine_reason, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                &row.instance_id,
                &row.generation,
                &row.repo_id,
                &row.common_dir,
                &row.admin_id,
                &row.branch_ref,
                &row.path,
                &row.owner,
                &row.state,
                &row.quarantine_reason,
                row.created_at,
            ],
        )?;
        Ok(())
    }

    /// Read one opaque instance claim. Malformed BLOB lengths are surfaced as
    /// an error rather than becoming a partially trusted identity.
    pub fn worktree_instance(&self, instance_id: &[u8]) -> Result<Option<WorktreeInstanceRow>> {
        if instance_id.len() != INSTANCE_ID_BYTES {
            bail!("instance_id must contain exactly {INSTANCE_ID_BYTES} bytes");
        }
        let row = self
            .conn()
            .query_row(
                &format!("SELECT {COLUMNS} FROM worktree_instances WHERE instance_id=?1"),
                [instance_id],
                decode,
            )
            .optional()?;
        row.map(|value| {
            validate_row(&value)?;
            Ok(value)
        })
        .transpose()
    }

    /// Return every claim for an exact path. There is deliberately no `LIMIT
    /// 1`: callers must quarantine ambiguity instead of selecting a winner.
    pub fn worktree_instances_for_path(&self, path: &[u8]) -> Result<Vec<WorktreeInstanceRow>> {
        if path.is_empty() || path.len() > MAX_IDENTITY_FIELD_BYTES {
            bail!("path is empty or exceeds the identity bound");
        }
        let mut stmt = self.conn().prepare(&format!(
            "SELECT {COLUMNS} FROM worktree_instances WHERE path=?1 ORDER BY created_at, instance_id"
        ))?;
        let rows = stmt.query_map([path], decode)?;
        let mut out = Vec::new();
        for row in rows {
            let row = row?;
            validate_row(&row)?;
            out.push(row);
        }
        Ok(out)
    }

    /// Transition an exact claim to quarantine or a recoverable split state.
    /// The reason is mandatory so a read-only UI can explain why admission was
    /// refused.
    pub fn quarantine_worktree_instance(
        &self,
        instance_id: &[u8],
        state: &str,
        reason: &str,
    ) -> Result<bool> {
        if instance_id.len() != INSTANCE_ID_BYTES {
            bail!("instance_id must contain exactly {INSTANCE_ID_BYTES} bytes");
        }
        if !matches!(state, "quarantined" | "split") || reason.trim().is_empty() {
            bail!("quarantine state requires a non-empty reason");
        }
        let changed = self.conn().execute(
            "UPDATE worktree_instances SET state=?2, quarantine_reason=?3 WHERE instance_id=?1",
            params![instance_id, state, reason],
        )?;
        Ok(changed == 1)
    }

    /// Promote a legacy ledger claim only after its caller has performed exact
    /// Git admission. This is intentionally a small state transition, not a
    /// schema migration with hidden filesystem I/O.
    pub fn verify_worktree_instance(&self, instance_id: &[u8]) -> Result<bool> {
        if instance_id.len() != INSTANCE_ID_BYTES {
            bail!("instance_id must contain exactly {INSTANCE_ID_BYTES} bytes");
        }
        let changed = self.conn().execute(
            "UPDATE worktree_instances
                SET state='verified', quarantine_reason=NULL
              WHERE instance_id=?1 AND state='legacy'",
            [instance_id],
        )?;
        Ok(changed == 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(seed: u8, path: &[u8]) -> WorktreeInstanceRow {
        WorktreeInstanceRow {
            instance_id: vec![seed; INSTANCE_ID_BYTES],
            generation: vec![seed.wrapping_add(1); GENERATION_BYTES],
            repo_id: vec![seed.wrapping_add(2); REPOSITORY_ID_BYTES],
            common_dir: b"/repo/.git".to_vec(),
            admin_id: vec![seed.wrapping_add(3); 4],
            branch_ref: b"feat/a".to_vec(),
            path: path.to_vec(),
            owner: b"test".to_vec(),
            state: "legacy".into(),
            quarantine_reason: None,
            created_at: i64::from(seed),
        }
    }

    #[test]
    fn v69_schema_is_additive_and_legacy_rows_are_not_backfilled() {
        let db = Db::open_memory().unwrap();
        let worktree_columns: Vec<String> = db
            .conn()
            .prepare("SELECT name FROM pragma_table_info('worktrees')")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .map(|row| row.unwrap())
            .collect();
        assert!(worktree_columns.iter().any(|name| name == "instance_id"));
        assert!(worktree_columns.iter().any(|name| name == "identity_state"));
        assert!(
            worktree_columns
                .iter()
                .any(|name| name == "quarantine_reason")
        );
        let ledger_count: i64 = db
            .conn()
            .query_row("SELECT count(*) FROM worktree_instances", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(
            ledger_count, 0,
            "schema migration must not probe or claim Git worktrees"
        );
        let group_columns: Vec<String> = db
            .conn()
            .prepare("SELECT name FROM pragma_table_info('tab_groups')")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .map(|row| row.unwrap())
            .collect();
        assert!(group_columns.iter().any(|name| name == "instance_id"));
        assert!(group_columns.iter().any(|name| name == "identity_state"));
        assert!(group_columns.iter().any(|name| name == "quarantine_reason"));
    }

    #[test]
    fn exact_ledger_reads_validate_lengths_and_never_select_a_first_path_claim() {
        let db = Db::open_memory().unwrap();
        let first = row(1, b"/repo/.worktrees/one");
        db.put_worktree_instance(&first).unwrap();
        assert_eq!(
            db.worktree_instances_for_path(b"/repo/.worktrees/one")
                .unwrap(),
            vec![first.clone()]
        );
        assert!(
            db.worktree_instance(&[1; INSTANCE_ID_BYTES])
                .unwrap()
                .is_some()
        );
        assert!(db.worktree_instance(&[1; 3]).is_err());

        let duplicate_path = row(2, b"/repo/.worktrees/one");
        assert!(db.put_worktree_instance(&duplicate_path).is_err());
        assert_eq!(
            db.worktree_instances_for_path(b"/repo/.worktrees/one")
                .unwrap()
                .len(),
            1,
            "a uniqueness conflict is never resolved by LIMIT 1"
        );
        assert!(
            db.quarantine_worktree_instance(
                &[1; INSTANCE_ID_BYTES],
                "quarantined",
                "ambiguous path"
            )
            .unwrap()
        );
        let quarantined = db
            .worktree_instance(&[1; INSTANCE_ID_BYTES])
            .unwrap()
            .unwrap();
        assert_eq!(quarantined.state, "quarantined");
        assert_eq!(
            quarantined.quarantine_reason.as_deref(),
            Some("ambiguous path")
        );
        assert!(
            db.verify_worktree_instance(&[1; INSTANCE_ID_BYTES])
                .unwrap()
                == false
        );
    }
}
