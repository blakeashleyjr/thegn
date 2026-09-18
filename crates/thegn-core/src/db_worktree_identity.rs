//! Exact worktree identity ledger access.
//!
//! The ledger is intentionally separate from the legacy path/tab registry.
//! Schema creation does not inspect Git and this module never treats a display
//! label or a lossy path as an authority key. Admission callers must first
//! inspect Git, then write a verified row or explicitly quarantine it.

use crate::db::Db;
use crate::models::WorktreeInstanceRow;
use anyhow::{Result, bail};
use rusqlite::{
    OptionalExtension, Row, params,
    types::{Type, ValueRef},
};

pub(crate) const INSTANCE_ID_BYTES: usize = 32;
pub(crate) const GENERATION_BYTES: usize = 16;
pub(crate) const REPOSITORY_ID_BYTES: usize = 32;
pub(crate) const MAX_IDENTITY_FIELD_BYTES: usize = 16 * 1024;
const MAX_REASON_BYTES: usize = 1024;
const MAX_PATH_CLAIMS: usize = 1024;
const MAX_PATH_CLAIM_BYTES: usize = 16 * 1024 * 1024;

/// Expected generation and operation revision for a compare-and-set ledger
/// transition. The generation is stable across rename; the revision changes on
/// every published operation so stale same-generation work cannot commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpectedWorktreeRevision {
    generation: Vec<u8>,
    operation_revision: i64,
}

impl ExpectedWorktreeRevision {
    pub fn new(generation: &[u8], operation_revision: i64) -> Result<Self> {
        if generation.len() != GENERATION_BYTES || operation_revision < 0 {
            bail!("invalid expected worktree generation or operation revision");
        }
        Ok(Self {
            generation: generation.to_vec(),
            operation_revision,
        })
    }
}

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
    if let Some(branch) = row.branch_ref.as_deref()
        && (branch.is_empty() || branch.len() > MAX_IDENTITY_FIELD_BYTES || branch.contains(&0))
    {
        bail!("branch_ref is invalid or exceeds the identity bound");
    }
    if row.operation_revision < 0 {
        bail!("operation_revision must not be negative");
    }
    match row.state.as_str() {
        "verified" | "legacy" if row.quarantine_reason.is_none() => Ok(()),
        "quarantined" | "split" => {
            let reason = row
                .quarantine_reason
                .as_deref()
                .filter(|r| !r.trim().is_empty())
                .ok_or_else(|| anyhow::anyhow!("quarantined worktree claims require a reason"))?;
            if reason.len() > MAX_REASON_BYTES {
                bail!("quarantine reason exceeds its bound");
            }
            Ok(())
        }
        _ => bail!("invalid worktree identity state or quarantine reason"),
    }
}

fn malformed(index: usize, message: &str) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        index,
        Type::Blob,
        Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            message,
        )),
    )
}

fn ensure_revision_can_advance(expected: &ExpectedWorktreeRevision) -> Result<()> {
    if expected.operation_revision == i64::MAX {
        bail!("worktree operation revision is exhausted");
    }
    Ok(())
}

fn bounded_blob(
    row: &Row<'_>,
    index: usize,
    name: &str,
    exact: Option<usize>,
) -> rusqlite::Result<Vec<u8>> {
    let bytes = match row.get_ref(index)? {
        ValueRef::Blob(bytes) => bytes,
        _ => return Err(malformed(index, &format!("{name} is not a BLOB"))),
    };
    if bytes.is_empty()
        || bytes.len() > MAX_IDENTITY_FIELD_BYTES
        || exact.is_some_and(|n| bytes.len() != n)
    {
        return Err(malformed(
            index,
            &format!("{name} exceeds its stored bound"),
        ));
    }
    Ok(bytes.to_vec())
}

fn optional_branch(row: &Row<'_>) -> rusqlite::Result<Option<Vec<u8>>> {
    match row.get_ref(5)? {
        ValueRef::Null => Ok(None),
        ValueRef::Blob(bytes)
            if !bytes.is_empty()
                && bytes.len() <= MAX_IDENTITY_FIELD_BYTES
                && !bytes.contains(&0) =>
        {
            Ok(Some(bytes.to_vec()))
        }
        _ => Err(malformed(5, "branch_ref exceeds its stored bound")),
    }
}

fn bounded_text(row: &Row<'_>, index: usize, name: &str, max: usize) -> rusqlite::Result<String> {
    let bytes = match row.get_ref(index)? {
        ValueRef::Text(bytes) => bytes,
        _ => return Err(malformed(index, &format!("{name} is not TEXT"))),
    };
    if bytes.is_empty() || bytes.len() > max {
        return Err(malformed(
            index,
            &format!("{name} exceeds its stored bound"),
        ));
    }
    std::str::from_utf8(bytes)
        .map(str::to_owned)
        .map_err(|_| malformed(index, &format!("{name} is not UTF-8")))
}

fn optional_reason(row: &Row<'_>) -> rusqlite::Result<Option<String>> {
    match row.get_ref(9)? {
        ValueRef::Null => Ok(None),
        ValueRef::Text(bytes) if !bytes.is_empty() && bytes.len() <= MAX_REASON_BYTES => {
            std::str::from_utf8(bytes)
                .map(|r| Some(r.to_owned()))
                .map_err(|_| malformed(9, "quarantine_reason is not UTF-8"))
        }
        _ => Err(malformed(9, "quarantine_reason exceeds its stored bound")),
    }
}

fn decode(row: &Row<'_>) -> rusqlite::Result<WorktreeInstanceRow> {
    Ok(WorktreeInstanceRow {
        instance_id: bounded_blob(row, 0, "instance_id", Some(INSTANCE_ID_BYTES))?,
        generation: bounded_blob(row, 1, "generation", Some(GENERATION_BYTES))?,
        repo_id: bounded_blob(row, 2, "repo_id", Some(REPOSITORY_ID_BYTES))?,
        common_dir: bounded_blob(row, 3, "common_dir", None)?,
        admin_id: bounded_blob(row, 4, "admin_id", None)?,
        branch_ref: optional_branch(row)?,
        path: bounded_blob(row, 6, "path", None)?,
        owner: bounded_blob(row, 7, "owner", None)?,
        state: bounded_text(row, 8, "state", 32)?,
        quarantine_reason: optional_reason(row)?,
        operation_revision: row.get(10)?,
        created_at: row.get(11)?,
    })
}

const COLUMNS: &str = "instance_id, generation, repo_id, common_dir, admin_id, branch_ref, path, owner, state, quarantine_reason, operation_revision, created_at";

impl Db {
    pub fn put_worktree_instance(&self, row: &WorktreeInstanceRow) -> Result<()> {
        validate_row(row)?;
        self.conn().execute(
            "INSERT INTO worktree_instances
               (instance_id, generation, repo_id, common_dir, admin_id, branch_ref, path, owner, state, quarantine_reason, operation_revision, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![&row.instance_id, &row.generation, &row.repo_id, &row.common_dir, &row.admin_id,
                &row.branch_ref, &row.path, &row.owner, &row.state, &row.quarantine_reason,
                row.operation_revision, row.created_at],
        )?;
        Ok(())
    }

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

    pub fn worktree_instances_for_path(&self, path: &[u8]) -> Result<Vec<WorktreeInstanceRow>> {
        if path.is_empty() || path.len() > MAX_IDENTITY_FIELD_BYTES {
            bail!("path is empty or exceeds the identity bound");
        }
        let mut stmt = self.conn().prepare(&format!(
            "SELECT {COLUMNS} FROM worktree_instances WHERE path=?1 ORDER BY created_at, instance_id LIMIT ?2"
        ))?;
        let limit = i64::try_from(MAX_PATH_CLAIMS + 1).expect("identity claim limit fits SQLite");
        let rows = stmt.query_map(params![path, limit], decode)?;
        let mut out = Vec::new();
        let mut total_bytes = 0usize;
        for (index, row) in rows.enumerate() {
            let row = row?;
            validate_row(&row)?;
            if index >= MAX_PATH_CLAIMS {
                bail!("worktree path has too many identity claims");
            }
            let row_bytes = row
                .instance_id
                .len()
                .checked_add(row.generation.len())
                .and_then(|n| n.checked_add(row.repo_id.len()))
                .and_then(|n| n.checked_add(row.common_dir.len()))
                .and_then(|n| n.checked_add(row.admin_id.len()))
                .and_then(|n| n.checked_add(row.branch_ref.as_ref().map_or(0, Vec::len)))
                .and_then(|n| n.checked_add(row.path.len()))
                .and_then(|n| n.checked_add(row.owner.len()))
                .and_then(|n| n.checked_add(row.state.len()))
                .and_then(|n| n.checked_add(row.quarantine_reason.as_ref().map_or(0, String::len)))
                .ok_or_else(|| anyhow::anyhow!("worktree identity claim size overflow"))?;
            total_bytes = total_bytes
                .checked_add(row_bytes)
                .ok_or_else(|| anyhow::anyhow!("worktree identity claim size overflow"))?;
            if total_bytes > MAX_PATH_CLAIM_BYTES {
                bail!("worktree path identity claims exceed their aggregate bound");
            }
            out.push(row);
        }
        Ok(out)
    }

    /// Quarantine or split a claim with an exact generation/revision CAS.
    pub fn quarantine_worktree_instance(
        &self,
        instance_id: &[u8],
        expected: &ExpectedWorktreeRevision,
        state: &str,
        reason: &str,
    ) -> Result<bool> {
        if instance_id.len() != INSTANCE_ID_BYTES {
            bail!("instance_id must contain exactly {INSTANCE_ID_BYTES} bytes");
        }
        if !matches!(state, "quarantined" | "split")
            || reason.trim().is_empty()
            || reason.len() > MAX_REASON_BYTES
        {
            bail!("quarantine state requires a bounded non-empty reason");
        }
        ensure_revision_can_advance(expected)?;
        let changed = self.conn().execute(
            "UPDATE worktree_instances SET state=?3, quarantine_reason=?4, operation_revision=operation_revision+1
              WHERE instance_id=?1 AND generation=?2 AND operation_revision=?5",
            params![instance_id, &expected.generation, state, reason, expected.operation_revision],
        )?;
        Ok(changed == 1)
    }

    /// Promote a legacy claim only when its exact generation/revision is still
    /// current. A partial-index conflict returns an error; no row wins first.
    pub fn verify_worktree_instance(
        &self,
        instance_id: &[u8],
        expected: &ExpectedWorktreeRevision,
    ) -> Result<bool> {
        if instance_id.len() != INSTANCE_ID_BYTES {
            bail!("instance_id must contain exactly {INSTANCE_ID_BYTES} bytes");
        }
        ensure_revision_can_advance(expected)?;
        self.transaction(|db| {
            let Some((path, repo_id, admin_id, generation, revision, state, reason)) = db
                .conn()
                .query_row(
                    "SELECT path, repo_id, admin_id, generation, operation_revision, state, quarantine_reason
                       FROM worktree_instances WHERE instance_id=?1",
                    [instance_id],
                    |row| {
                        Ok((
                            row.get::<_, Vec<u8>>(0)?,
                            row.get::<_, Vec<u8>>(1)?,
                            row.get::<_, Vec<u8>>(2)?,
                            row.get::<_, Vec<u8>>(3)?,
                            row.get::<_, i64>(4)?,
                            row.get::<_, String>(5)?,
                            row.get::<_, Option<String>>(6)?,
                        ))
                    },
                )
                .optional()?
            else {
                return Ok(false);
            };
            if generation != expected.generation
                || revision != expected.operation_revision
                || state != "legacy"
                || reason.is_some()
            {
                return Ok(false);
            }
            if path.is_empty()
                || path.len() > MAX_IDENTITY_FIELD_BYTES
                || repo_id.len() != REPOSITORY_ID_BYTES
                || admin_id.is_empty()
                || admin_id.len() > MAX_IDENTITY_FIELD_BYTES
            {
                bail!("worktree identity claim is malformed");
            }
            let competing: i64 = db.conn().query_row(
                "SELECT count(*) FROM worktree_instances
                   WHERE instance_id != ?1
                     AND (path = ?2 OR (repo_id = ?3 AND admin_id = ?4))",
                params![instance_id, &path, &repo_id, &admin_id],
                |row| row.get(0),
            )?;
            if competing != 0 {
                bail!("worktree identity has unresolved competing claims");
            }
            let changed = db.conn().execute(
                "UPDATE worktree_instances SET state='verified', quarantine_reason=NULL, operation_revision=operation_revision+1
                  WHERE instance_id=?1 AND generation=?2 AND operation_revision=?3 AND state='legacy' AND quarantine_reason IS NULL",
                params![instance_id, &expected.generation, expected.operation_revision],
            )?;
            Ok(changed == 1)
        })
    }

    pub fn advance_worktree_operation_revision(
        &self,
        instance_id: &[u8],
        expected: &ExpectedWorktreeRevision,
    ) -> Result<bool> {
        if instance_id.len() != INSTANCE_ID_BYTES {
            bail!("instance_id must contain exactly {INSTANCE_ID_BYTES} bytes");
        }
        ensure_revision_can_advance(expected)?;
        let changed = self.conn().execute(
            "UPDATE worktree_instances SET operation_revision=operation_revision+1 WHERE instance_id=?1 AND generation=?2 AND operation_revision=?3",
            params![instance_id, &expected.generation, expected.operation_revision],
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
            branch_ref: Some(b"feat/a".to_vec()),
            path: path.to_vec(),
            owner: b"test".to_vec(),
            state: "legacy".into(),
            quarantine_reason: None,
            operation_revision: 0,
            created_at: i64::from(seed),
        }
    }

    #[test]
    fn competing_legacy_claims_never_promote_first_wins() {
        let db = Db::open_memory().unwrap();
        let first = row(1, b"/repo/.worktrees/one");
        let second = row(2, b"/repo/.worktrees/one");
        db.put_worktree_instance(&first).unwrap();
        db.put_worktree_instance(&second).unwrap();
        assert_eq!(
            db.worktree_instances_for_path(first.path.as_slice())
                .unwrap()
                .len(),
            2
        );
        let expected = ExpectedWorktreeRevision::new(&first.generation, 0).unwrap();
        assert!(
            db.verify_worktree_instance(&first.instance_id, &expected)
                .is_err()
        );
        let expected_second = ExpectedWorktreeRevision::new(&second.generation, 0).unwrap();
        assert!(
            db.verify_worktree_instance(&second.instance_id, &expected_second)
                .is_err()
        );
        assert!(
            db.worktree_instance(&first.instance_id)
                .unwrap()
                .unwrap()
                .state
                == "legacy"
        );
    }

    #[test]
    fn detached_claims_and_revision_transitions_are_bounded() {
        let db = Db::open_memory().unwrap();
        let mut detached = row(3, b"/repo/.worktrees/detached");
        detached.branch_ref = None;
        db.put_worktree_instance(&detached).unwrap();
        let expected = ExpectedWorktreeRevision::new(&detached.generation, 0).unwrap();
        assert!(
            db.advance_worktree_operation_revision(&detached.instance_id, &expected)
                .unwrap()
        );
        assert!(
            !db.advance_worktree_operation_revision(&detached.instance_id, &expected)
                .unwrap()
        );
        let current = db
            .worktree_instance(&detached.instance_id)
            .unwrap()
            .unwrap();
        assert_eq!(current.branch_ref, None);
        assert_eq!(current.operation_revision, 1);
    }

    #[test]
    fn revision_maximum_is_rejected_before_sqlite_can_promote_it() {
        let db = Db::open_memory().unwrap();
        let mut row = row(4, b"/repo/.worktrees/max");
        row.operation_revision = i64::MAX;
        db.put_worktree_instance(&row).unwrap();
        let expected = ExpectedWorktreeRevision::new(&row.generation, i64::MAX).unwrap();
        assert!(
            db.advance_worktree_operation_revision(&row.instance_id, &expected)
                .is_err()
        );
        assert_eq!(
            db.worktree_instance(&row.instance_id)
                .unwrap()
                .unwrap()
                .operation_revision,
            i64::MAX
        );
    }

    #[test]
    fn path_claim_listing_has_exact_and_over_limit_behavior() {
        let db = Db::open_memory().unwrap();
        for index in 0..=MAX_PATH_CLAIMS {
            let mut claim = row(5, b"/repo/.worktrees/many");
            claim.instance_id[0] = (index & 0xff) as u8;
            claim.instance_id[1] = (index >> 8) as u8;
            claim.generation[0] = (index & 0xff) as u8;
            claim.created_at = index as i64;
            db.put_worktree_instance(&claim).unwrap();
        }
        assert!(
            db.worktree_instances_for_path(b"/repo/.worktrees/many")
                .is_err()
        );
        // A fresh database with exactly the cap is still a complete result,
        // rather than an arbitrary SQL prefix.
        let exact = Db::open_memory().unwrap();
        for index in 0..MAX_PATH_CLAIMS {
            let mut claim = row(6, b"/repo/.worktrees/exact");
            claim.instance_id[0] = (index & 0xff) as u8;
            claim.instance_id[1] = (index >> 8) as u8;
            claim.generation[0] = (index & 0xff) as u8;
            claim.created_at = index as i64;
            exact.put_worktree_instance(&claim).unwrap();
        }
        assert_eq!(
            exact
                .worktree_instances_for_path(b"/repo/.worktrees/exact")
                .unwrap()
                .len(),
            MAX_PATH_CLAIMS
        );

        let malformed = Db::open_memory().unwrap();
        malformed
            .conn()
            .execute(
                "INSERT INTO worktree_instances
                 (instance_id,generation,repo_id,common_dir,admin_id,branch_ref,path,owner,state,operation_revision,created_at)
                 VALUES (?1,?2,?3,zeroblob(?4),?5,?6,?7,?8,'legacy',0,0)",
                params![
                    vec![7u8; INSTANCE_ID_BYTES],
                    vec![8u8; GENERATION_BYTES],
                    vec![9u8; REPOSITORY_ID_BYTES],
                    MAX_IDENTITY_FIELD_BYTES + 1,
                    vec![10u8; 4],
                    b"branch".as_slice(),
                    b"/repo/.worktrees/oversized".as_slice(),
                    b"test".as_slice(),
                ],
            )
            .unwrap();
        assert!(
            malformed
                .worktree_instances_for_path(b"/repo/.worktrees/oversized")
                .is_err()
        );
    }
}
