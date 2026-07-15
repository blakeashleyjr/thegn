//! WorktreeAuxStore state — the embedded-SQLite implementation of the [`WorktreeAuxStore`] seam.
//! Sibling `impl` block (via the `conn()` accessor) so the pinned `db.rs`
//! only carries the schema DDL, not these bodies. The DB is a cache; git /
//! the live source is truth. A server backend implements this trait against
//! Postgres for shared, multi-user state.

use crate::db::{Db, ForwardRow, MergeQueueRow, ShareRow};
use crate::models::ContainerEvent;
use crate::store::WorktreeAuxStore;
use crate::util;
use anyhow::Result;
use rusqlite::params;

impl WorktreeAuxStore for Db {
    // --- registers (persisted yank registers, v27) ------------------------
    /// Persist a register's value (upsert). The single-char `name` is the
    /// register id; the volatile `+` clipboard register is never stored here.
    fn put_register(&self, name: char, value: &str) -> Result<()> {
        self.conn().execute(
            "INSERT INTO registers(name,value,updated_at) VALUES(?1,?2,?3)
             ON CONFLICT(name) DO UPDATE SET value=excluded.value, updated_at=excluded.updated_at",
            params![name.to_string(), value.as_bytes(), util::now()],
        )?;
        Ok(())
    }

    /// Load every persisted register as `(name, value)` pairs.
    fn all_registers(&self) -> Result<Vec<(char, String)>> {
        let mut stmt = self.conn().prepare("SELECT name, value FROM registers")?;
        let rows = stmt.query_map([], |r| {
            let name: String = r.get(0)?;
            let value: Vec<u8> = r.get(1)?;
            Ok((name, value))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (name, value) = row?;
            if let Some(ch) = name.chars().next() {
                out.push((ch, String::from_utf8_lossy(&value).into_owned()));
            }
        }
        Ok(out)
    }

    // --- ingress shares (`[share]`; resurrection layer for tunnels) --------
    /// Insert or update the share record for `(worktree, local_port)`.
    fn upsert_share(
        &self,
        worktree: &str,
        local_port: u16,
        provider: &str,
        public_url: Option<&str>,
        state: &str,
    ) -> Result<()> {
        self.conn().execute(
            "INSERT INTO shares(worktree,local_port,provider,public_url,state,created_at)
             VALUES(?1,?2,?3,?4,?5,?6)
             ON CONFLICT(worktree,local_port) DO UPDATE SET
               provider=excluded.provider,
               public_url=excluded.public_url,
               state=excluded.state",
            params![
                worktree,
                local_port as i64,
                provider,
                public_url,
                state,
                util::now()
            ],
        )?;
        Ok(())
    }

    /// All persisted shares, newest first (restore + panel listing).
    fn list_shares(&self) -> Result<Vec<ShareRow>> {
        let mut stmt = self.conn().prepare(
            "SELECT worktree,local_port,provider,public_url,state,created_at \
             FROM shares ORDER BY created_at DESC",
        )?;
        let rows = stmt
            .query_map([], Self::map_share_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Remove the share record for `(worktree, local_port)`.
    fn delete_share(&self, worktree: &str, local_port: u16) -> Result<()> {
        self.conn().execute(
            "DELETE FROM shares WHERE worktree=?1 AND local_port=?2",
            params![worktree, local_port as i64],
        )?;
        Ok(())
    }

    // --- auto port forwards (`[forward]`; resurrection layer) --------------
    /// Insert or update the forward record for `(worktree, container_port)`.
    fn upsert_forward(
        &self,
        worktree: &str,
        container_port: u16,
        host_port: u16,
        url: &str,
    ) -> Result<()> {
        self.conn().execute(
            "INSERT INTO forwards(worktree,container_port,host_port,url,created_at)
             VALUES(?1,?2,?3,?4,?5)
             ON CONFLICT(worktree,container_port) DO UPDATE SET
               host_port=excluded.host_port,
               url=excluded.url",
            params![
                worktree,
                container_port as i64,
                host_port as i64,
                url,
                util::now()
            ],
        )?;
        Ok(())
    }

    /// All persisted forwards, newest first (restore + panel listing).
    fn list_forwards(&self) -> Result<Vec<ForwardRow>> {
        let mut stmt = self.conn().prepare(
            "SELECT worktree,container_port,host_port,url,created_at \
             FROM forwards ORDER BY created_at DESC",
        )?;
        let rows = stmt
            .query_map([], Self::map_forward_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Remove the forward record for `(worktree, container_port)`.
    fn delete_forward(&self, worktree: &str, container_port: u16) -> Result<()> {
        self.conn().execute(
            "DELETE FROM forwards WHERE worktree=?1 AND container_port=?2",
            params![worktree, container_port as i64],
        )?;
        Ok(())
    }

    /// Enqueue (or re-enqueue) a worktree branch for the next fold. Re-enqueueing
    /// resets the row to `queued` and clears any prior result/conflict/error, so
    /// a branch that was deferred and then rebased starts fresh.
    fn enqueue_merge(&self, worktree: &str, branch: &str, target_branch: &str) -> Result<()> {
        let now = util::now();
        // `location` mirrors `worktrees.location` at enqueue time via a
        // correlated subquery (NULL when the worktree isn't registered → treated
        // as local), so the queue self-describes each row's host without a
        // separate write path.
        self.conn().execute(
            r#"INSERT INTO merge_queue
                 (worktree,branch,target_branch,status,queued_at,updated_at,
                  result_oid,conflict_paths,error_detail,location)
               VALUES(?1,?2,?3,'queued',?4,?4,NULL,NULL,NULL,
                  (SELECT location FROM worktrees WHERE worktree=?1))
               ON CONFLICT(worktree) DO UPDATE SET
                 branch=?2, target_branch=?3, status='queued',
                 queued_at=?4, updated_at=?4,
                 result_oid=NULL, conflict_paths=NULL, error_detail=NULL,
                 location=(SELECT location FROM worktrees WHERE worktree=?1)"#,
            params![worktree, branch, target_branch, now],
        )?;
        Ok(())
    }

    /// Update a queued worktree's status and (optionally) its result oid,
    /// conflicted paths (newline-joined), and error detail. Passing `None` leaves
    /// the corresponding column unchanged.
    fn update_merge_status(
        &self,
        worktree: &str,
        status: &str,
        result_oid: Option<&str>,
        conflict_paths: Option<&str>,
        error_detail: Option<&str>,
    ) -> Result<()> {
        self.conn().execute(
            r#"UPDATE merge_queue SET
                 status=?2, updated_at=?3,
                 result_oid=COALESCE(?4, result_oid),
                 conflict_paths=COALESCE(?5, conflict_paths),
                 error_detail=COALESCE(?6, error_detail)
               WHERE worktree=?1"#,
            params![
                worktree,
                status,
                util::now(),
                result_oid,
                conflict_paths,
                error_detail
            ],
        )?;
        Ok(())
    }

    /// Drop a worktree's merge-queue row (e.g. after a clean land is recorded
    /// elsewhere, or the worktree is removed).
    fn remove_merge_entry(&self, worktree: &str) -> Result<()> {
        self.conn().execute(
            "DELETE FROM merge_queue WHERE worktree=?1",
            params![worktree],
        )?;
        Ok(())
    }

    /// The whole queue, oldest-queued first (the fold order + UI feed).
    fn list_merge_queue(&self) -> Result<Vec<MergeQueueRow>> {
        let mut stmt = self.conn().prepare(
            r#"SELECT worktree,branch,target_branch,status,queued_at,updated_at,
                      result_oid,conflict_paths,error_detail,location
               FROM merge_queue ORDER BY queued_at"#,
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(MergeQueueRow {
                worktree: r.get(0)?,
                branch: r.get(1)?,
                target_branch: r.get(2)?,
                status: r.get(3)?,
                queued_at: r.get(4)?,
                updated_at: r.get(5)?,
                result_oid: r.get(6)?,
                conflict_paths: r.get(7)?,
                error_detail: r.get(8)?,
                // NULL (pre-v44 / unregistered worktree) = local / same store.
                location: r.get::<_, Option<String>>(9)?.unwrap_or_default(),
            })
        })?;
        let mut v = Vec::new();
        for row in rows {
            v.push(row?);
        }
        Ok(v)
    }

    // --- per-worktree disk usage cache (v20) -------------------------------
    /// `(size_bytes, target_bytes, fetched_at)` for one worktree, or `None`.
    fn get_worktree_disk(&self, worktree: &str) -> Result<Option<(i64, i64, i64)>> {
        let r = self
            .conn()
            .query_row(
                "SELECT size_bytes, target_bytes, fetched_at FROM worktree_disk WHERE worktree=?1",
                params![worktree],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, i64>(2)?,
                    ))
                },
            )
            .ok();
        Ok(r)
    }

    fn put_worktree_disk(&self, worktree: &str, size_bytes: i64, target_bytes: i64) -> Result<()> {
        self.conn().execute(
            r#"INSERT INTO worktree_disk(worktree,size_bytes,target_bytes,fetched_at)
               VALUES(?1,?2,?3,?4)
               ON CONFLICT(worktree) DO UPDATE SET size_bytes=?2, target_bytes=?3, fetched_at=?4"#,
            params![worktree, size_bytes, target_bytes, util::now()],
        )?;
        Ok(())
    }

    /// All cached disk sizes keyed by worktree path → `(size_bytes, target_bytes)`.
    /// One bulk read for the sidebar/statusbar; never scans.
    fn all_worktree_disk(&self) -> Result<std::collections::HashMap<String, (i64, i64)>> {
        let mut stmt = self
            .conn()
            .prepare("SELECT worktree, size_bytes, target_bytes FROM worktree_disk")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                (r.get::<_, i64>(1)?, r.get::<_, i64>(2)?),
            ))
        })?;
        let mut map = std::collections::HashMap::new();
        for row in rows {
            let (k, v) = row?;
            map.insert(k, v);
        }
        Ok(map)
    }

    /// Drop a worktree's cached size (e.g. right after a `clean`) so the badge
    /// clears without waiting for the next scan.
    fn delete_worktree_disk(&self, worktree: &str) -> Result<()> {
        self.conn().execute(
            "DELETE FROM worktree_disk WHERE worktree=?1",
            params![worktree],
        )?;
        Ok(())
    }

    // --- worktree↔issue links (badge + palette surfacing) -------------------
    /// Associate `issue_id` (in `"<provider>:<key>"` form) with a worktree path.
    fn link_issue(&self, worktree_path: &str, issue_id: &str) -> Result<()> {
        self.conn().execute(
            r#"INSERT INTO issue_links(worktree_path,issue_id,linked_at)
               VALUES(?1,?2,?3)
               ON CONFLICT(worktree_path,issue_id) DO UPDATE SET linked_at=?3"#,
            params![worktree_path, issue_id, util::now()],
        )?;
        Ok(())
    }

    /// Remove a worktree↔issue association.
    fn unlink_issue(&self, worktree_path: &str, issue_id: &str) -> Result<()> {
        self.conn().execute(
            "DELETE FROM issue_links WHERE worktree_path=?1 AND issue_id=?2",
            params![worktree_path, issue_id],
        )?;
        Ok(())
    }

    /// All issue ids linked to a worktree, newest first.
    fn linked_issues(&self, worktree_path: &str) -> Result<Vec<String>> {
        let mut stmt = self.conn().prepare(
            "SELECT issue_id FROM issue_links WHERE worktree_path=?1 ORDER BY linked_at DESC",
        )?;
        let rows = stmt.query_map(params![worktree_path], |r| r.get::<_, String>(0))?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    // --- reflog-undo bookkeeping (which resets are OURS, per worktree) ------
    /// Record a reset target we are about to create, pruning each worktree's
    /// mark set to the freshest 100 (the undo planner only reads ~100 reflog
    /// entries anyway).
    fn add_undo_mark(&self, worktree: &str, sha: &str) -> Result<()> {
        self.conn().execute(
            r#"INSERT INTO undo_marks(worktree,sha,ts) VALUES(?1,?2,?3)
               ON CONFLICT(worktree,sha) DO UPDATE SET ts=?3"#,
            params![worktree, sha, util::now()],
        )?;
        self.conn().execute(
            r#"DELETE FROM undo_marks WHERE worktree=?1 AND sha NOT IN (
                 SELECT sha FROM undo_marks WHERE worktree=?1
                 ORDER BY ts DESC LIMIT 100)"#,
            params![worktree],
        )?;
        Ok(())
    }

    /// All recorded undo-reset targets for a worktree (newest first).
    fn undo_marks(&self, worktree: &str) -> Result<Vec<String>> {
        let mut stmt = self
            .conn()
            .prepare("SELECT sha FROM undo_marks WHERE worktree=?1 ORDER BY ts DESC")?;
        let rows = stmt.query_map(params![worktree], |r| r.get::<_, String>(0))?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// Record a sandbox event (exec, network, dns, orphan_gc) in the audit log.
    fn insert_container_event(
        &self,
        worktree: &str,
        ts: i64,
        kind: &str,
        detail: Option<&str>,
        exit_code: Option<i64>,
    ) -> Result<()> {
        self.conn().execute(
            "INSERT INTO container_events (worktree, ts, kind, detail, exit_code)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![worktree, ts, kind, detail, exit_code],
        )?;
        Ok(())
    }

    /// Retrieve the most recent `limit` container events for a worktree,
    /// newest first.
    fn container_events(&self, worktree: &str, limit: usize) -> Result<Vec<ContainerEvent>> {
        let mut stmt = self.conn().prepare(
            "SELECT id, worktree, ts, kind, detail, exit_code
             FROM container_events
             WHERE worktree = ?1
             ORDER BY ts DESC, id DESC
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![worktree, limit as i64], |r| {
            Ok(ContainerEvent {
                id: r.get(0)?,
                worktree: r.get(1)?,
                ts: r.get(2)?,
                kind: r.get(3)?,
                detail: r.get(4)?,
                exit_code: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Delete container events older than `older_than_secs` seconds. Called on
    /// startup to keep the audit table from growing unbounded.
    fn prune_container_events(&self, older_than_secs: i64) -> Result<usize> {
        let cutoff = crate::util::now() - older_than_secs;
        let n = self.conn().execute(
            "DELETE FROM container_events WHERE ts < ?1",
            params![cutoff],
        )?;
        Ok(n)
    }
}
