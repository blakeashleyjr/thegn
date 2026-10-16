//! WorkspaceStore state — the embedded-SQLite implementation of the [`WorkspaceStore`] seam.
//! Sibling `impl` block (via the `conn()` accessor) so the pinned `db.rs`
//! only carries the schema DDL, not these bodies. The DB is a cache; git /
//! the live source is truth. A server backend implements this trait against
//! Postgres for shared, multi-user state.

use crate::db::Db;
use crate::db::session;
use crate::models::{WorkspaceRow, WorktreeRow};
use crate::store::WorkspaceStore;
use crate::util;
use anyhow::Result;
use rusqlite::{OptionalExtension, params};

pub(crate) const ORPHAN_CACHE_SWEEP_MAX_KEYS: usize = 1024;

const WORKTREE_RECORD_COLUMNS: &str = "worktree, COALESCE(branch,''), COALESCE(agent,''), COALESCE(created_at,0), COALESCE(repo_path,''), COALESCE(tab_name,''), session_name, location, position, sandbox_backend, observed_backend, folder_id, env_name";

fn decode_worktree_record(r: &rusqlite::Row<'_>) -> rusqlite::Result<WorktreeRow> {
    Ok(WorktreeRow {
        worktree: r.get(0)?,
        branch: r.get(1)?,
        agent: r.get(2)?,
        created_at: r.get(3)?,
        repo_root: r.get(4)?,
        tab_name: r.get(5)?,
        session_name: r.get::<_, Option<String>>(6)?.unwrap_or_default(),
        location: r.get::<_, Option<String>>(7)?.unwrap_or_default(),
        position: r.get::<_, Option<i64>>(8)?.unwrap_or(0),
        sandbox_backend: r.get(9)?,
        observed_backend: r.get(10)?,
        folder_id: r.get(11)?,
        env_name: r
            .get::<_, Option<String>>(12)?
            .filter(|s| !s.trim().is_empty()),
    })
}

pub(crate) fn delete_worktree_cache_rows(db: &Db, wt: &str) -> Result<()> {
    let base_conn = db.conn();
    // Keep standalone cascades atomic, while joining a caller's transaction
    // when this helper is used during transactional cleanup.
    let tx = if base_conn.is_autocommit() {
        Some(base_conn.unchecked_transaction()?)
    } else {
        None
    };
    let conn = tx.as_deref().unwrap_or(base_conn);
    let prefix = if wt.ends_with('/') {
        wt.to_string()
    } else {
        format!("{wt}/")
    };
    // '/' sorts immediately before '0', so this range includes descendants
    // while excluding sibling prefixes such as `/wt/a2`.
    let prefix_end = format!("{wt}0");
    let has_glyph_cache = table_exists(conn, "glyph_cache")?;
    for table in [
        "worktree_disk",
        "loc_cache",
        "attention_acks",
        "session_attention",
        "pr_cache",
        "pr_review_cache",
        "ci_runs_cache",
        "ci_log_cache",
        "ci_autofix_dedupe",
        "commit_cache",
        "worktree_titles",
        "test_cache",
        "diff_cache",
        "undo_marks",
    ] {
        let column = if matches!(table, "attention_acks" | "session_attention") {
            "worktree_path"
        } else {
            "worktree"
        };
        conn.execute(
            &format!("DELETE FROM {table} WHERE {column}=?1"),
            params![wt],
        )?;
    }
    // Resolve matching entity IDs once through idx_sem_entity_file. Reuse that
    // set for edge cleanup and entity deletion in this cascade.
    let entity_ids = {
        let mut stmt = conn.prepare(
            "SELECT id FROM sem_entity
             WHERE file=?1 OR (file>=?2 AND file<?3)",
        )?;
        let rows = stmt.query_map(params![wt, prefix, prefix_end], |row| {
            row.get::<_, String>(0)
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    for id in entity_ids {
        conn.execute("DELETE FROM sem_edge WHERE src_id=?1", params![id])?;
        conn.execute("DELETE FROM sem_edge WHERE dst_id=?1", params![id])?;
        conn.execute("DELETE FROM sem_entity WHERE id=?1", params![id])?;
    }
    // glyph_cache is lazy-created and may not exist on a fresh database.
    if has_glyph_cache {
        conn.execute("DELETE FROM glyph_cache WHERE worktree=?1", params![wt])?;
    }
    if let Some(tx) = tx {
        tx.commit()?;
    }
    Ok(())
}

impl Db {
    /// Prune cached rows for up to `ORPHAN_CACHE_SWEEP_MAX_KEYS` worktree keys
    /// that are neither registered nor present on disk. This is called from the
    /// terminal slice of `prune_missing_semantic_files`, which the `db.rs`
    /// worker loop reaches after its semantic scan is complete.
    pub(crate) fn prune_orphan_worktree_caches(&self, max_keys: usize) -> Result<usize> {
        let conn = self.conn();
        let mut cache_queries = vec![
            "SELECT worktree FROM worktree_disk",
            "SELECT worktree FROM loc_cache",
            "SELECT worktree_path FROM attention_acks",
            "SELECT worktree_path FROM session_attention",
            "SELECT worktree FROM pr_cache",
            "SELECT worktree FROM pr_review_cache",
            "SELECT worktree FROM ci_runs_cache",
            "SELECT worktree FROM ci_log_cache",
            "SELECT worktree FROM ci_autofix_dedupe",
            "SELECT worktree FROM commit_cache",
            "SELECT worktree FROM worktree_titles",
            "SELECT worktree FROM test_cache",
            "SELECT worktree FROM diff_cache",
            "SELECT worktree FROM undo_marks",
        ];
        if table_exists(conn, "glyph_cache")? {
            cache_queries.push("SELECT worktree FROM glyph_cache");
        }
        let query = format!(
            "SELECT DISTINCT worktree FROM ({}) WHERE worktree IS NOT NULL AND worktree != '' ORDER BY worktree",
            cache_queries.join(" UNION ALL ")
        );
        let keys = {
            let mut stmt = conn.prepare(&query)?;
            let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        let registered = {
            let mut stmt = conn.prepare(
                "SELECT worktree FROM worktrees WHERE worktree != '' \
                 UNION SELECT repo_path FROM worktrees WHERE repo_path != '' \
                 UNION SELECT repo_path FROM workspaces WHERE repo_path != ''",
            )?;
            let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
            rows.collect::<rusqlite::Result<std::collections::HashSet<_>>>()?
        };
        let orphan_keys: Vec<String> = keys
            .into_iter()
            .filter(|key| {
                !registered.contains(key)
                    && !registered.iter().any(|root| {
                        std::path::Path::new(root).starts_with(std::path::Path::new(key))
                    })
                    && !std::path::Path::new(key).exists()
            })
            .take(max_keys)
            .collect();
        for key in &orphan_keys {
            delete_worktree_cache_rows(self, key)?;
        }

        // Repo-keyed cache rows use the same registrations, ancestor, and
        // on-disk protections. Share the sweep's key budget so this pass
        // cannot make one terminal slice unbounded. The return value remains
        // the number of worktree-cache keys removed, preserving its contract.
        let remaining_budget = max_keys.saturating_sub(orphan_keys.len());
        if remaining_budget > 0 {
            let repo_cache_tables = [
                ("issue_cache", "repo_root"),
                ("pr_branch_cache", "repo_root"),
                ("issue_projects", "repo_root"),
            ];
            let repo_cache_queries: Vec<String> = repo_cache_tables
                .iter()
                .map(|(table, column)| format!("SELECT {column} FROM {table}"))
                .collect();
            let repo_query = format!(
                "SELECT DISTINCT repo_root FROM ({}) \
                 WHERE repo_root IS NOT NULL AND repo_root != '' ORDER BY repo_root",
                repo_cache_queries.join(" UNION ALL ")
            );
            let repo_roots = {
                let mut stmt = conn.prepare(&repo_query)?;
                let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
                rows.collect::<rusqlite::Result<Vec<_>>>()?
            };
            let orphan_repo_roots: Vec<String> = repo_roots
                .into_iter()
                .filter(|key| {
                    !registered.contains(key)
                        && !registered.iter().any(|root| {
                            std::path::Path::new(root).starts_with(std::path::Path::new(key))
                        })
                        && !std::path::Path::new(key).exists()
                })
                .take(remaining_budget)
                .collect();
            for repo_root in orphan_repo_roots {
                for (table, column) in repo_cache_tables {
                    conn.execute(
                        &format!("DELETE FROM {table} WHERE {column}=?1"),
                        params![repo_root],
                    )?;
                }
            }
        }
        Ok(orphan_keys.len())
    }
}

fn table_exists(conn: &rusqlite::Connection, table: &str) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
        params![table],
        |row| row.get(0),
    )?)
}

impl WorkspaceStore for Db {
    // --- repo history (launcher recents) -----------------------------------
    fn touch_repo(&self, path: &str, name: &str) -> Result<()> {
        let now = util::now();
        // `seq` is a monotonic logical clock so recents ordering stays correct
        // even when several repos are opened in the same wall-clock second.
        self.conn().execute(
            r#"
            INSERT INTO repos(path,name,first_seen,last_opened,open_count,seq)
              VALUES(?1,?2,?3,?3,1,(SELECT COALESCE(MAX(seq),0)+1 FROM repos))
            ON CONFLICT(path) DO UPDATE SET
              last_opened=?3, open_count=open_count+1, name=?2,
              seq=(SELECT COALESCE(MAX(seq),0)+1 FROM repos)
            "#,
            params![path, name, now],
        )?;
        Ok(())
    }

    fn recent_repos(&self, limit: i64) -> Result<Vec<String>> {
        let mut stmt = self
            .conn()
            .prepare("SELECT path FROM repos ORDER BY seq DESC LIMIT ?1")?;
        let rows = stmt.query_map([limit], |r| r.get::<_, String>(0))?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    fn known_repos(&self) -> Result<Vec<String>> {
        let mut stmt = self.conn().prepare(
            "SELECT repo_path FROM worktrees
             UNION SELECT repo_path FROM workspaces
             UNION SELECT path FROM repos",
        )?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        Ok(rows
            .filter_map(|r| r.ok())
            .filter(|s| !s.is_empty())
            .collect())
    }

    /// Whether thegn already knows this repo (registered, or in recents).
    fn is_known_repo(&self, repo_path: &str) -> Result<bool> {
        let found: i64 = self
            .conn()
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM workspaces WHERE repo_path=?1)
                     OR EXISTS(SELECT 1 FROM repos WHERE path=?1)",
                params![repo_path],
                |r| r.get(0),
            )
            .unwrap_or(0);
        Ok(found != 0)
    }

    // --- workspaces (a registered repo or plain dir) ----------------------
    /// Record (or refresh) a registered workspace. Keyed by path — all
    /// workspaces share the one UI session. `kind` is `"repo"` (a git repo) or
    /// `"dir"` (a plain non-git directory); it is set only on first insert, so a
    /// later refresh never downgrades a known workspace's kind.
    fn put_workspace(&self, repo_path: &str, name: &str, kind: &str) -> Result<()> {
        let now = util::now();
        self.conn().execute(
            r#"INSERT INTO workspaces(repo_path,name,created_at,last_active,kind,position)
               VALUES(?1,?2,?3,?3,?4,(SELECT COALESCE(MAX(position),-1)+1 FROM workspaces))
               ON CONFLICT(repo_path) DO UPDATE SET name=?2, last_active=?3"#,
            params![repo_path, name, now, kind],
        )?;
        Ok(())
    }

    /// A stable, globally-unique slug for a repo (the prefix of all its tabs).
    /// Reuses the previously-assigned slug; otherwise takes `base`, suffixing
    /// `-2`, `-3`, … on collision with a *different* repo, then persists it.
    /// Two repos with the same basename therefore get distinct tab namespaces.
    fn slug_for_repo(&self, repo_path: &str, base: &str) -> Result<String> {
        // One transaction around the read-check-insert so two processes can't
        // both pass the uniqueness scan and claim the same slug.
        self.transaction(|db| {
            if let Ok(s) = db.conn().query_row(
                "SELECT slug FROM repo_slugs WHERE repo_path=?1",
                params![repo_path],
                |r| r.get::<_, String>(0),
            ) && !s.is_empty()
            {
                return Ok(s);
            }
            let taken: std::collections::HashSet<String> = {
                let mut stmt = db
                    .conn()
                    .prepare("SELECT slug FROM repo_slugs WHERE repo_path != ?1")?;
                let rows = stmt.query_map(params![repo_path], |r| r.get::<_, String>(0))?;
                rows.filter_map(|r| r.ok()).collect()
            };
            let mut cand = base.to_string();
            let mut n = 1;
            while taken.contains(&cand) {
                n += 1;
                cand = format!("{base}-{n}");
            }
            db.conn().execute(
                "INSERT OR REPLACE INTO repo_slugs(repo_path, slug) VALUES(?1, ?2)",
                params![repo_path, cand],
            )?;
            Ok(cand)
        })
    }

    fn repo_slug_rows(&self) -> Result<Vec<(String, String)>> {
        let mut stmt = self
            .conn()
            .prepare("SELECT repo_path, slug FROM repo_slugs ORDER BY repo_path")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Drop a repo's stable sidebar slug so a removed workspace can't reclaim a
    /// stale slug if it is reopened later.
    fn del_repo_slug(&self, repo_path: &str) -> Result<()> {
        self.conn().execute(
            "DELETE FROM repo_slugs WHERE repo_path=?1",
            params![repo_path],
        )?;
        Ok(())
    }

    /// Forget a whole workspace (no disk side effects). Removes the
    /// `workspaces` row so the sidebar stops listing it. The worktree files on
    /// disk are intentionally left untouched.
    fn del_workspace(&self, repo_path: &str) -> Result<()> {
        self.conn().execute(
            "DELETE FROM workspaces WHERE repo_path=?1",
            params![repo_path],
        )?;
        // A workspace's MAIN checkout is a measured path in its own right (the
        // background size scan covers `workspaces.repo_path`, not just the
        // `worktrees` registry — a home group never gets a `worktrees` row), so
        // its caches are keyed by `repo_path` and cascade here rather than in
        // `del_worktrees_for_repo`.
        self.conn().execute(
            "DELETE FROM worktree_disk WHERE worktree=?1",
            params![repo_path],
        )?;
        self.conn().execute(
            "DELETE FROM loc_cache WHERE worktree=?1",
            params![repo_path],
        )?;
        Ok(())
    }

    /// All registered repos (for the sidebar / `list`), in manual `position`
    /// order (seeded from recency at the v16 migration; reorderable via
    /// `swap_workspace_positions`). The `last_active DESC` tie-break keeps the
    /// order deterministic if any row's position is somehow NULL.
    fn workspaces(&self) -> Result<Vec<WorkspaceRow>> {
        let mut stmt = self.conn().prepare(
            "SELECT repo_path, name, created_at, last_active, kind
             FROM workspaces ORDER BY position, last_active DESC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(WorkspaceRow {
                repo_path: r.get(0)?,
                name: r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                created_at: r.get::<_, Option<i64>>(2)?.unwrap_or(0),
                last_active: r.get::<_, Option<i64>>(3)?.unwrap_or(0),
                kind: r
                    .get::<_, Option<String>>(4)?
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| "repo".into()),
            })
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// Swap the persisted sort positions of two workspaces (by repo_path). The
    /// workspace analogue of `swap_worktree_positions`: the sidebar's manual
    /// workspace reorder (Ctrl+Alt+↑/↓) picks two adjacent workspaces and this
    /// exchanges their `position` so the new order survives restart.
    ///
    /// `position` is not always distinct + non-NULL: insert paths that bypass
    /// `put_workspace` (`migrate_brand`, `db_zones`) never set it, and backfill
    /// ties can duplicate it. A naive value-swap then no-ops (NULL, or equal
    /// values), so the reorder never reached the DB and the nav ring — rebuilt
    /// from `workspaces()` order on the next hydration — kept walking the old
    /// order. So when either side is NULL or the two are equal, first normalize
    /// the whole table to contiguous distinct positions (preserving the current
    /// display order), then swap. Wrapped in one transaction so a normalize +
    /// swap is atomic.
    fn swap_workspace_positions(&self, a: &str, b: &str) -> Result<()> {
        self.transaction(|db| {
            // Read both before writing — a self-referential CASE-UPDATE could
            // observe its own intermediate write and clobber the swap.
            let pos = |p: &str| -> Result<Option<i64>> {
                Ok(db
                    .conn()
                    .query_row(
                        "SELECT position FROM workspaces WHERE repo_path=?1",
                        params![p],
                        |r| r.get::<_, Option<i64>>(0),
                    )
                    .optional()?
                    .flatten())
            };
            let (mut pa, mut pb) = (pos(a)?, pos(b)?);
            if pa.is_none() || pb.is_none() || pa == pb {
                db.normalize_workspace_positions()?;
                (pa, pb) = (pos(a)?, pos(b)?);
            }
            if let (Some(pa), Some(pb)) = (pa, pb) {
                db.set_workspace_position(a, pb)?;
                db.set_workspace_position(b, pa)?;
            }
            Ok(())
        })
    }

    /// Set one workspace's persisted sort position (repo_path key).
    fn set_workspace_position(&self, repo_path: &str, position: i64) -> Result<()> {
        self.conn().execute(
            "UPDATE workspaces SET position=?2 WHERE repo_path=?1",
            params![repo_path, position],
        )?;
        Ok(())
    }

    /// Persist the full sidebar workspace order (see the trait docs). Writes
    /// `position = index` for each path in `order` under one transaction, so
    /// the persisted order is exactly the sequence the caller (the sidebar,
    /// after a manual reorder) is showing — the reload via `workspaces()` can't
    /// drift from it the way a swap + `normalize_workspace_positions` re-query
    /// can when positions are NULL/tied.
    fn set_workspace_order(&self, order: &[String]) -> Result<()> {
        self.transaction(|db| {
            for (i, p) in order.iter().enumerate() {
                db.set_workspace_position(p, i as i64)?;
            }
            Ok(())
        })
    }

    // --- worktrees (one per tab; keyed by worktree path) -------------------
    /// Record a worktree. `location` is the remote descriptor (JSON) for a remote
    /// worktree, or `None`/empty for an ordinary on-host one.
    fn put_worktree(
        &self,
        tab: &str,
        root: &str,
        wt: &str,
        branch: &str,
        location: Option<&str>,
        folder_id: Option<i64>,
    ) -> Result<()> {
        // Insert unconditionally falls to the end (`MAX+1`), while upsert leaves an
        // existing `position` untouched so a re-register never reshuffles order.
        // `location` COALESCEs for the same reason: re-register callers pass None
        // (they don't know the placement), which must not wipe a persisted
        // provider/ssh location — removal deletes the whole row instead.
        self.conn().execute(
            r#"INSERT INTO worktrees(worktree,session_name,tab_name,repo_path,branch,agent,created_at,location,position,folder_id)
               VALUES(?1,?2,?3,?4,?5,'',?6,?7,(SELECT COALESCE(MAX(position),-1)+1 FROM worktrees),?8)
               ON CONFLICT(worktree) DO UPDATE SET branch=?5, tab_name=?3, repo_path=?4, session_name=?2, agent=COALESCE(agent,''), created_at=COALESCE(created_at,?6), location=COALESCE(?7, location), folder_id=COALESCE(?8, folder_id)"#,
            params![wt, session(), tab, root, branch, util::now(), location, folder_id],
        )?;
        Ok(())
    }

    fn set_worktree_location(&self, wt: &str, location: &str) -> Result<()> {
        self.conn().execute(
            "UPDATE worktrees SET location=?2 WHERE worktree=?1",
            params![wt, location],
        )?;
        Ok(())
    }

    /// The remote-location descriptor for a worktree (None/empty = local).
    fn location_for(&self, wt: &str) -> Result<Option<String>> {
        self.conn()
            .query_row(
                "SELECT location FROM worktrees WHERE worktree=?1",
                params![wt],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()
            .map(|row| row.flatten())
            .map_err(Into::into)
    }

    /// The (local) repo root recorded for a worktree — needed for the per-repo
    /// `.thegn` overlay when the worktree itself lives remote.
    fn repo_root_for(&self, wt: &str) -> Result<Option<String>> {
        self.conn()
            .query_row(
                "SELECT repo_path FROM worktrees WHERE worktree=?1",
                params![wt],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()
            .map(|row| row.flatten())
            .map_err(Into::into)
    }

    /// The recorded agent for a worktree (for `pick-agent --resume` on restart).
    fn worktree_agent(&self, worktree: &str) -> Result<Option<String>> {
        let r = self
            .conn()
            .query_row(
                "SELECT agent FROM worktrees WHERE worktree=?1",
                params![worktree],
                |r| r.get::<_, String>(0),
            )
            .ok()
            .filter(|s: &String| !s.is_empty());
        Ok(r)
    }

    fn set_worktree_agent(&self, wt: &str, agent: &str) -> Result<()> {
        self.conn().execute(
            "UPDATE worktrees SET agent=?2 WHERE worktree=?1",
            params![wt, agent],
        )?;
        Ok(())
    }

    fn del_worktree(&self, wt: &str) -> Result<()> {
        self.conn()
            .execute("DELETE FROM worktrees WHERE worktree=?1", params![wt])?;
        delete_worktree_cache_rows(self, wt)?;
        // And the per-worktree status caches + queue row. Every delete path
        // (interactive close, the registry reap for a dir removed outside
        // thegn, the stale-group prune) funnels through here — before this,
        // only the interactive path removed the merge-queue row, so `git
        // worktree remove` in a shell left the MQ badge counting a branch
        // that no longer exists, and a worktree recreated at the same path
        // inherited the old one's CI failure (an instant ✋) and stale glyphs.
        // A recognized cleanup hold is independent retained branch history,
        // not a live-worktree cache. Generic hydration/reaping must not erase
        // it; explicit queue dismissal still uses remove_merge_entry.
        self.conn().execute(
            "DELETE FROM merge_queue WHERE worktree=?1 AND NOT(status='landed' AND error_detail IS ?2)",
            params![wt, crate::merge_sweep::CleanupHold::BranchRetained.marker()],
        )?;
        Ok(())
    }

    /// Forget every registry worktree row owned by a repo (no disk side
    /// effects). Pairs with [`Self::del_workspace`] so a removed workspace's
    /// cross-workspace rows neither re-render nor resurrect on the next launch.
    fn del_worktrees_for_repo(&self, repo_path: &str) -> Result<()> {
        let worktrees = {
            let mut stmt = self
                .conn()
                .prepare("SELECT worktree FROM worktrees WHERE repo_path=?1")?;
            let rows = stmt.query_map(params![repo_path], |row| row.get::<_, String>(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        for wt in &worktrees {
            delete_worktree_cache_rows(self, wt)?;
        }
        self.conn().execute(
            "DELETE FROM worktrees WHERE repo_path=?1",
            params![repo_path],
        )?;
        Ok(())
    }

    /// Re-key a worktree registry row after a rename (`git branch -m` +
    /// `git worktree move`): the primary key `worktree` (path) moves to
    /// `new_path`, and the `tab_name`/`branch` follow the new branch. `position`,
    /// `agent`, and `sandbox_backend` are preserved. No-op if the old row is gone.
    fn rename_worktree(
        &self,
        old_path: &str,
        new_path: &str,
        new_tab: &str,
        new_branch: &str,
    ) -> Result<()> {
        self.conn().execute(
            "UPDATE worktrees SET worktree=?2, tab_name=?3, branch=?4 WHERE worktree=?1",
            params![old_path, new_path, new_tab, new_branch],
        )?;
        Ok(())
    }

    /// Forget the registry row for a worktree group by its owning repo and tab
    /// name. This is intentionally independent of the worktree path so close /
    /// delete operations cannot be undone by a stale row whose path was moved or
    /// normalized differently than the live session group.
    fn del_worktree_for_tab(&self, repo_root: &str, tab: &str) -> Result<()> {
        let worktrees = {
            let mut stmt = self
                .conn()
                .prepare("SELECT worktree FROM worktrees WHERE repo_path=?1 AND tab_name=?2")?;
            let rows = stmt.query_map(params![repo_root, tab], |row| row.get::<_, String>(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        for wt in &worktrees {
            delete_worktree_cache_rows(self, wt)?;
        }
        self.conn().execute(
            "DELETE FROM worktrees WHERE repo_path=?1 AND tab_name=?2",
            params![repo_root, tab],
        )?;
        Ok(())
    }

    fn set_worktree_sandbox(&self, wt: &str, backend: &str) -> Result<()> {
        self.conn().execute(
            "UPDATE worktrees SET sandbox_backend=?2 WHERE worktree=?1",
            params![wt, backend],
        )?;
        Ok(())
    }

    fn set_worktree_observed(&self, wt: &str, backend: &str) -> Result<()> {
        self.conn().execute(
            "UPDATE worktrees SET observed_backend=?2 WHERE worktree=?1",
            params![wt, backend],
        )?;
        Ok(())
    }

    fn worktree_observed(&self, wt: &str) -> Result<Option<String>> {
        let mut stmt = self
            .conn()
            .prepare("SELECT observed_backend FROM worktrees WHERE worktree=?1")?;
        let mut rows = stmt.query(params![wt])?;
        if let Some(row) = rows.next()? {
            let val: Option<String> = row.get(0)?;
            Ok(val)
        } else {
            Ok(None)
        }
    }

    fn worktree_sandbox(&self, wt: &str) -> Result<Option<String>> {
        let mut stmt = self
            .conn()
            .prepare("SELECT sandbox_backend FROM worktrees WHERE worktree=?1")?;
        let mut rows = stmt.query(params![wt])?;
        if let Some(row) = rows.next()? {
            let val: Option<String> = row.get(0)?;
            Ok(val)
        } else {
            Ok(None)
        }
    }

    /// The worktree path for a (session, tab) pair — how the panel plugin maps
    /// the focused tab to a worktree (PaneInfo carries no cwd).
    fn worktree_for_tab(&self, session: &str, tab: &str) -> Result<Option<String>> {
        // THE-516: never pick the first of several rows. Legacy slugged tab
        // names can alias distinct worktrees; an ambiguous tab is refused so
        // no caller routes to (or mutates) an arbitrary claimant.
        let res = self
            .conn()
            .prepare(
                "SELECT worktree FROM worktrees WHERE session_name=?1 AND tab_name=?2 \
                 ORDER BY worktree LIMIT 2",
            )
            .and_then(|mut stmt| {
                stmt.query_map(params![session, tab], |r| r.get::<_, String>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()
            });
        // best-effort read: a real DB error is surfaced but still degrades to
        // a miss (the pane maps to nothing).
        let rows = match res {
            Ok(rows) => rows,
            Err(e) => {
                tracing::warn!(target: "thegn::db", error = %e, "worktree_for_tab read failed; treating as a miss");
                return Ok(None);
            }
        };
        if rows.len() > 1 {
            anyhow::bail!("tab {tab} is claimed by several worktrees; refusing to pick one");
        }
        Ok(rows.into_iter().next())
    }

    /// All recorded worktrees (metadata only; git supplies live status).
    fn worktrees(&self) -> Result<Vec<WorktreeRow>> {
        // `position` is the persistent sort key (creation order by default,
        // user-reorderable). Order by it so every consumer — the sidebar's
        // unloaded-workspace rows and the resurrect adopt loop — is stable;
        // created_at/path are deterministic tie-breakers for any unset row.
        // COALESCE the NOT-NULL-in-Rust columns: `set_worktree_env` can insert a
        // minimal row (only `worktree` + `env_name`) before the worktree is first
        // opened, leaving branch/agent/created_at/repo_path/tab_name NULL. Without
        // the COALESCE, `r.get::<_, String/i64>` errors on those NULLs and the row
        // is silently dropped by `filter_map(|r| r.ok())` — losing the env pin and
        // any row `put_worktree` hasn't fully healed yet.
        let mut stmt = self.conn().prepare(&format!(
            "SELECT {WORKTREE_RECORD_COLUMNS} FROM worktrees ORDER BY position, created_at, worktree"
        ))?;
        let rows = stmt.query_map([], decode_worktree_record)?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    fn worktree_record(&self, worktree: &str) -> Result<Option<WorktreeRow>> {
        self.conn()
            .query_row(
                &format!("SELECT {WORKTREE_RECORD_COLUMNS} FROM worktrees WHERE worktree=?1"),
                [worktree],
                decode_worktree_record,
            )
            .optional()
            .map_err(Into::into)
    }

    /// Swap the persisted sort positions of two worktrees (by path). Used by
    /// the sidebar's manual reorder (Shift+Alt+↑/↓): the caller picks the two
    /// adjacent siblings, this exchanges their `position` so the new order
    /// survives restart. Same NULL/duplicate hazard as the workspace swap
    /// (v8-added column, inserts that miss it) — heal to contiguous distinct
    /// positions before swapping so a reorder never silently no-ops.
    fn swap_worktree_positions(&self, a: &str, b: &str) -> Result<()> {
        self.transaction(|db| {
            // Read both first, then write — a single CASE-UPDATE that reads the
            // table it mutates can observe its own intermediate write and clobber
            // the swap.
            let pos = |wt: &str| -> Result<Option<i64>> {
                Ok(db
                    .conn()
                    .query_row(
                        "SELECT position FROM worktrees WHERE worktree=?1",
                        params![wt],
                        |r| r.get::<_, Option<i64>>(0),
                    )
                    .optional()?
                    .flatten())
            };
            let (mut pa, mut pb) = (pos(a)?, pos(b)?);
            if pa.is_none() || pb.is_none() || pa == pb {
                db.normalize_worktree_positions()?;
                (pa, pb) = (pos(a)?, pos(b)?);
            }
            if let (Some(pa), Some(pb)) = (pa, pb) {
                db.set_worktree_position(a, pb)?;
                db.set_worktree_position(b, pa)?;
            }
            Ok(())
        })
    }

    /// Set one worktree's persisted sort position (path key). The session-layout
    /// persist path uses this to keep `position` in step with the live group
    /// order after a manual move.
    fn set_worktree_position(&self, wt: &str, position: i64) -> Result<()> {
        self.conn().execute(
            "UPDATE worktrees SET position=?2 WHERE worktree=?1",
            params![wt, position],
        )?;
        Ok(())
    }

    /// Persist the full sidebar worktree order (see the trait docs). One
    /// transaction so a reorder can never half-apply, and a plain
    /// `position = index` write so the reload through `worktrees()` reproduces
    /// the caller's sequence verbatim — no normalize/tiebreak drift.
    fn set_worktree_order(&self, order: &[String]) -> Result<()> {
        self.transaction(|db| {
            for (i, wt) in order.iter().enumerate() {
                db.set_worktree_position(wt, i as i64)?;
            }
            Ok(())
        })
    }

    fn folders_for_workspace(&self, repo_path: &str) -> Result<Vec<crate::models::FolderRow>> {
        let mut stmt = self.conn().prepare(
            "SELECT folder_id, repo_path, name, position, created_at
             FROM folders WHERE repo_path = ?1 ORDER BY position",
        )?;
        let rows = stmt.query_map(params![repo_path], |r| {
            Ok(crate::models::FolderRow {
                folder_id: r.get(0)?,
                repo_path: r.get(1)?,
                name: r.get(2)?,
                position: r.get(3)?,
                created_at: r.get(4)?,
            })
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    fn create_folder(&self, repo_path: &str, name: &str) -> Result<i64> {
        let created_at = crate::util::now();
        self.conn().execute(
            "INSERT INTO folders(repo_path, name, position, created_at)
             VALUES(?1, ?2, (SELECT COALESCE(MAX(position), -1) + 1 FROM folders WHERE repo_path = ?1), ?3)",
            params![repo_path, name, created_at],
        )?;
        Ok(self.conn().last_insert_rowid())
    }

    fn rename_folder(&self, folder_id: i64, new_name: &str) -> Result<()> {
        self.conn().execute(
            "UPDATE folders SET name = ?1 WHERE folder_id = ?2",
            params![new_name, folder_id],
        )?;
        Ok(())
    }

    /// Persist the folder order within a workspace (see the trait docs). The
    /// `repo_path` predicate keeps a stale/foreign id from renumbering another
    /// workspace's folders.
    fn set_folder_order(&self, repo_path: &str, order: &[i64]) -> Result<()> {
        self.transaction(|db| {
            for (i, fid) in order.iter().enumerate() {
                db.conn().execute(
                    "UPDATE folders SET position = ?1 WHERE folder_id = ?2 AND repo_path = ?3",
                    params![i as i64, fid, repo_path],
                )?;
            }
            Ok(())
        })
    }

    fn del_folder(&self, folder_id: i64) -> Result<()> {
        let tx = self.conn().unchecked_transaction()?;
        tx.execute(
            "UPDATE worktrees SET folder_id = NULL WHERE folder_id = ?1",
            params![folder_id],
        )?;
        tx.execute(
            "UPDATE terminals SET folder_id = NULL WHERE folder_id = ?1",
            params![folder_id],
        )?;
        tx.execute(
            "DELETE FROM folders WHERE folder_id = ?1",
            params![folder_id],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Find a folder in `repo_path` whose name matches `name`
    /// (case-insensitive, trimmed) and return its id, creating it if absent.
    /// This is the find-or-create primitive behind the "file worktree into
    /// folder" actions, so repeated firing never spawns duplicate folders.
    fn ensure_folder(
        &self,
        repo_path: &str,
        name: &str,
        workspace_name: &str,
        workspace_kind: &str,
    ) -> Result<i64> {
        let want = name.trim();
        self.transaction(|db| {
            // A failed tombstone read propagates through `?`: uncertainty must
            // never be treated as permission to resurrect a removed workspace.
            if db.workspace_tombstoned(repo_path)? {
                return Err(crate::store::WorkspaceTombstonedError {
                    repo_path: repo_path.to_string(),
                }
                .into());
            }
            // Insert-if-absent: folder bookkeeping must never modify an
            // existing workspace row (name, kind, position, last_active).
            db.conn().execute(
                r#"INSERT INTO workspaces(repo_path,name,created_at,last_active,kind,position)
                   VALUES(?1,?2,?3,?3,?4,(SELECT COALESCE(MAX(position),-1)+1 FROM workspaces))
                   ON CONFLICT(repo_path) DO NOTHING"#,
                params![repo_path, workspace_name, util::now(), workspace_kind],
            )?;
            for folder in db.folders_for_workspace(repo_path)? {
                if folder.name.trim().eq_ignore_ascii_case(want) {
                    return Ok(folder.folder_id);
                }
            }
            db.create_folder(repo_path, want)
        })
    }

    /// File (or unfile, with `None`) a single worktree into a folder.
    fn set_worktree_folder(&self, worktree: &str, folder_id: Option<i64>) -> Result<()> {
        self.conn().execute(
            "UPDATE worktrees SET folder_id = ?1 WHERE worktree = ?2",
            params![folder_id, worktree],
        )?;
        Ok(())
    }

    fn set_worktree_folder_if_identity(
        &self,
        worktree: &str,
        repo_path: &str,
        folder_id: i64,
        expected_name: &str,
    ) -> Result<bool> {
        let changed = self.conn().execute(
            "UPDATE worktrees SET folder_id = ?1
             WHERE worktree = ?2 AND repo_path = ?3
               AND EXISTS (
                   SELECT 1 FROM folders
                   WHERE folder_id = ?1 AND repo_path = ?3 AND name = ?4
               )",
            params![folder_id, worktree, repo_path, expected_name],
        )?;
        Ok(changed != 0)
    }

    /// Select the named execution environment for a worktree (`[env.<name>]`).
    /// `""` clears it (inherit the workspace/repo/global layer).
    fn set_worktree_env(&self, wt: &str, env: &str) -> Result<()> {
        let val = (!env.trim().is_empty()).then(|| env.trim().to_string());
        // UPSERT, not UPDATE: a freshly `git worktree add`ed path isn't in the DB
        // yet (put_worktree runs when it's first opened), so a plain UPDATE was a
        // silent no-op and the pin never resolved — the "menu / `env set` selection
        // is ignored on a new worktree" bug. Insert a minimal row (git stays the
        // source of truth; the rest fills in on first open) so the pin sticks.
        self.conn().execute(
            "INSERT INTO worktrees(worktree, env_name) VALUES(?1, ?2) \
             ON CONFLICT(worktree) DO UPDATE SET env_name=?2",
            params![wt, val],
        )?;
        Ok(())
    }

    /// The worktree's selected env name, if any (NULL/empty ⇒ inherit).
    fn worktree_env(&self, wt: &str) -> Result<Option<String>> {
        let mut stmt = self
            .conn()
            .prepare("SELECT env_name FROM worktrees WHERE worktree=?1")?;
        let mut rows = stmt.query(params![wt])?;
        match rows.next()? {
            Some(row) => Ok(row
                .get::<_, Option<String>>(0)?
                .filter(|s| !s.trim().is_empty())),
            None => Ok(None),
        }
    }

    /// Select the default execution environment for a whole workspace. `""`
    /// clears it.
    fn set_workspace_env(&self, repo_path: &str, env: &str) -> Result<()> {
        let val = (!env.trim().is_empty()).then(|| env.trim().to_string());
        self.conn().execute(
            "UPDATE workspaces SET env_name=?2 WHERE repo_path=?1",
            params![repo_path, val],
        )?;
        Ok(())
    }

    /// The workspace's default env name, if any (NULL/empty ⇒ inherit).
    fn workspace_env(&self, repo_path: &str) -> Result<Option<String>> {
        let mut stmt = self
            .conn()
            .prepare("SELECT env_name FROM workspaces WHERE repo_path=?1")?;
        let mut rows = stmt.query(params![repo_path])?;
        match rows.next()? {
            Some(row) => Ok(row
                .get::<_, Option<String>>(0)?
                .filter(|s| !s.trim().is_empty())),
            None => Ok(None),
        }
    }

    /// The effective selected env for a worktree: its own `env_name`, else its
    /// workspace's `env_name`. (`None` ⇒ fall through to repo `.thegn.*` /
    /// global default in [`crate::config::Config::resolve_env`].)
    fn effective_env(&self, wt: &str, repo_path: &str) -> Option<String> {
        self.worktree_env(wt)
            .ok()
            .flatten()
            .or_else(|| self.workspace_env(repo_path).ok().flatten())
    }

    /// Save (or replace) a named layout snapshot. `spec` is a serialized
    /// `LayoutSpec` JSON string.
    fn put_layout(&self, name: &str, spec: &str) -> Result<()> {
        self.conn().execute(
            r#"INSERT INTO layouts(name, spec, created_at) VALUES(?1, ?2, ?3)
               ON CONFLICT(name) DO UPDATE SET spec=?2, created_at=?3"#,
            params![name, spec, util::now()],
        )?;
        Ok(())
    }

    /// The serialized spec for a named layout, if present.
    fn get_layout(&self, name: &str) -> Result<Option<String>> {
        let res = self.conn().query_row(
            "SELECT spec FROM layouts WHERE name=?1",
            params![name],
            |r| r.get::<_, String>(0),
        );
        // best-effort read: no rows is the None case; a real DB error is
        // surfaced but still degrades to a miss (the layout reads as absent).
        let r = match res {
            Ok(v) => Some(v),
            Err(rusqlite::Error::QueryReturnedNoRows) => None,
            Err(e) => {
                tracing::warn!(target: "thegn::db", error = %e, "get_layout read failed; treating as a miss");
                None
            }
        };
        Ok(r)
    }

    /// All saved layout names, alphabetical.
    fn list_layouts(&self) -> Result<Vec<String>> {
        let mut stmt = self
            .conn()
            .prepare("SELECT name FROM layouts ORDER BY name")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// Delete a named layout (no-op if absent).
    fn delete_layout(&self, name: &str) -> Result<()> {
        self.conn()
            .execute("DELETE FROM layouts WHERE name=?1", params![name])?;
        Ok(())
    }

    /// Insert or replace a worktree group's persisted row.
    fn put_tab_group(&self, session: &str, row: &crate::models::TabGroupRow) -> Result<()> {
        self.conn().execute(
            "INSERT INTO tab_groups
               (session_name, name, kind, worktree, ordinal, active_tab)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(session_name, name) DO UPDATE SET
               kind=?3, worktree=?4, ordinal=?5, active_tab=?6",
            params![
                session,
                row.name,
                row.kind,
                row.worktree,
                row.ordinal,
                row.active_tab,
            ],
        )?;
        Ok(())
    }

    /// Insert or replace one tab inside a worktree group.
    fn put_group_tab(&self, session: &str, row: &crate::models::GroupTabRow) -> Result<()> {
        self.conn().execute(
            "INSERT INTO group_tabs
               (session_name, group_name, ordinal, title, pane_tree, focused_pane, pane_cwds, pane_cmds, pane_sessions, scrollback_snapshot)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(session_name, group_name, ordinal) DO UPDATE SET
               title=?4, pane_tree=?5, focused_pane=?6, pane_cwds=?7, pane_cmds=?8, pane_sessions=?9, scrollback_snapshot=?10",
            params![
                session,
                row.group_name,
                row.ordinal,
                row.title,
                row.pane_tree,
                row.focused_pane,
                row.pane_cwds,
                row.pane_cmds,
                row.pane_sessions,
                row.scrollback_snapshot,
            ],
        )?;
        Ok(())
    }

    /// All persisted worktree groups for a session, in display order.
    fn groups_for_session(&self, session: &str) -> Result<Vec<crate::models::TabGroupRow>> {
        let mut stmt = self.conn().prepare(
            "SELECT name, kind, worktree, ordinal, active_tab
               FROM tab_groups WHERE session_name=?1 ORDER BY ordinal",
        )?;
        let rows = stmt.query_map(params![session], |r| {
            Ok(crate::models::TabGroupRow {
                name: r.get(0)?,
                kind: r.get(1)?,
                worktree: r.get::<_, Option<String>>(2)?.unwrap_or_default(),
                ordinal: r.get::<_, Option<i64>>(3)?.unwrap_or(0),
                active_tab: r.get::<_, Option<i64>>(4)?.unwrap_or(0),
            })
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// All persisted tabs for every group in a session, ordered (group, tab).
    fn group_tabs_for_session(&self, session: &str) -> Result<Vec<crate::models::GroupTabRow>> {
        let mut stmt = self.conn().prepare(
            "SELECT group_name, ordinal, title, pane_tree, focused_pane, pane_cwds, pane_cmds, pane_sessions, scrollback_snapshot
               FROM group_tabs WHERE session_name=?1 ORDER BY group_name, ordinal",
        )?;
        let rows = stmt.query_map(params![session], |r| {
            Ok(crate::models::GroupTabRow {
                group_name: r.get(0)?,
                ordinal: r.get::<_, Option<i64>>(1)?.unwrap_or(0),
                title: r.get::<_, Option<String>>(2)?.unwrap_or_default(),
                pane_tree: r.get::<_, Option<String>>(3)?.unwrap_or_default(),
                focused_pane: r.get::<_, Option<i64>>(4)?.unwrap_or(0),
                pane_cwds: r.get::<_, Option<String>>(5)?.unwrap_or_default(),
                pane_cmds: r.get::<_, Option<String>>(6)?.unwrap_or_default(),
                pane_sessions: r.get::<_, Option<String>>(7)?.unwrap_or_default(),
                scrollback_snapshot: r.get::<_, Option<String>>(8)?.unwrap_or_default(),
            })
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// The persisted group row and tabs of the TERMINAL group `name`, plus the
    /// session that last held them — `None` when no session ever persisted it.
    ///
    /// Terminals are workspace-independent (the `terminals` registry is global
    /// and its sidebar row renders in every workspace), but their layout rows
    /// are keyed by whichever session was active when they were last persisted.
    /// Re-opening a terminal from a different workspace has to find that row
    /// wherever it landed, or the restore has no `pane_sessions` to warm-reattach
    /// with and silently forks a fresh shell. Terminal names are unique in the
    /// `terminals` table, so the lookup is unambiguous; the `kind` filter keeps a
    /// same-named worktree group from ever answering.
    ///
    /// The session name comes back so the caller can delete the donor rows once
    /// the group has moved — two sessions both claiming one terminal is how a
    /// cold resurrect would resurrect a phantom duplicate.
    #[allow(clippy::type_complexity)]
    fn terminal_group_tabs(
        &self,
        name: &str,
    ) -> Result<
        Option<(
            String,
            crate::models::TabGroupRow,
            Vec<crate::models::GroupTabRow>,
        )>,
    > {
        let mut stmt = self.conn().prepare(
            "SELECT t.session_name, g.ordinal, g.active_tab,
                    t.group_name, t.ordinal, t.title, t.pane_tree,
                    t.focused_pane, t.pane_cwds, t.pane_cmds, t.pane_sessions,
                    t.scrollback_snapshot
               FROM group_tabs t
               JOIN tab_groups g
                 ON g.session_name = t.session_name AND g.name = t.group_name
              WHERE g.kind = 'terminal' AND g.name = ?1
              ORDER BY t.session_name, t.ordinal",
        )?;
        let rows = stmt.query_map(params![name], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<i64>>(1)?.unwrap_or(0),
                r.get::<_, Option<i64>>(2)?.unwrap_or(0),
                crate::models::GroupTabRow {
                    group_name: r.get(3)?,
                    ordinal: r.get::<_, Option<i64>>(4)?.unwrap_or(0),
                    title: r.get::<_, Option<String>>(5)?.unwrap_or_default(),
                    pane_tree: r.get::<_, Option<String>>(6)?.unwrap_or_default(),
                    focused_pane: r.get::<_, Option<i64>>(7)?.unwrap_or(0),
                    pane_cwds: r.get::<_, Option<String>>(8)?.unwrap_or_default(),
                    pane_cmds: r.get::<_, Option<String>>(9)?.unwrap_or_default(),
                    pane_sessions: r.get::<_, Option<String>>(10)?.unwrap_or_default(),
                    scrollback_snapshot: r.get::<_, Option<String>>(11)?.unwrap_or_default(),
                },
            ))
        })?;
        // A terminal belongs to exactly one session, but a crash between the
        // migration's write and its donor delete could leave two. Keep the first
        // session's tabs whole rather than interleaving two layouts into one
        // group — the ORDER BY makes that choice deterministic.
        let mut owner: Option<(String, crate::models::TabGroupRow)> = None;
        let mut tabs = Vec::new();
        for (session, ordinal, active_tab, row) in rows.filter_map(|r| r.ok()) {
            match &owner {
                Some((s, _)) if *s != session => break,
                Some(_) => tabs.push(row),
                None => {
                    owner = Some((
                        session,
                        crate::models::TabGroupRow {
                            name: name.to_string(),
                            kind: "terminal".to_string(),
                            worktree: String::new(),
                            ordinal,
                            active_tab,
                        },
                    ));
                    tabs.push(row);
                }
            }
        }
        Ok(owner.map(|(s, g)| (s, g, tabs)))
    }

    /// Forget one worktree group and its tabs (on worktree close).
    fn delete_tab_group(&self, session: &str, name: &str) -> Result<()> {
        self.conn().execute(
            "DELETE FROM group_tabs WHERE session_name=?1 AND group_name=?2",
            params![session, name],
        )?;
        self.conn().execute(
            "DELETE FROM tab_groups WHERE session_name=?1 AND name=?2",
            params![session, name],
        )?;
        Ok(())
    }

    /// Forget every group (and its tabs) of `session` keyed by worktree path.
    fn delete_tab_groups_for_worktree(&self, session: &str, worktree: &str) -> Result<()> {
        self.conn().execute(
            "DELETE FROM group_tabs WHERE session_name=?1 AND group_name IN
               (SELECT name FROM tab_groups WHERE session_name=?1 AND worktree=?2)",
            params![session, worktree],
        )?;
        self.conn().execute(
            "DELETE FROM tab_groups WHERE session_name=?1 AND worktree=?2",
            params![session, worktree],
        )?;
        Ok(())
    }

    fn delete_tab_groups_for_worktree_all_sessions(&self, worktree: &str) -> Result<()> {
        // Delete children first. Callers put this pair in their surrounding
        // transaction so a failed bookkeeping pass cannot leave half a layout
        // behind.
        self.conn().execute(
            "DELETE FROM group_tabs
             WHERE EXISTS (
                 SELECT 1 FROM tab_groups
                 WHERE tab_groups.session_name = group_tabs.session_name
                   AND tab_groups.name = group_tabs.group_name
                   AND tab_groups.worktree = ?1
             )",
            [worktree],
        )?;
        self.conn()
            .execute("DELETE FROM tab_groups WHERE worktree=?1", [worktree])?;
        Ok(())
    }

    fn delete_tab_groups_with_prefix(&self, slug: &str) -> Result<Vec<(String, String)>> {
        let prefix = format!("{slug}/");
        self.transaction(|db| {
            let pane_sessions = {
                let mut statement = db.conn().prepare(
                    "SELECT group_tabs.session_name, group_tabs.pane_sessions
                     FROM group_tabs
                     WHERE group_tabs.pane_sessions != ''
                       AND EXISTS (
                           SELECT 1 FROM tab_groups
                           WHERE tab_groups.session_name = group_tabs.session_name
                             AND tab_groups.name = group_tabs.group_name
                             AND substr(tab_groups.name, 1, length(?1)) = ?1
                       )
                     ORDER BY group_tabs.session_name, group_tabs.group_name, group_tabs.ordinal",
                )?;
                let rows = statement.query_map([&prefix], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?;
                rows.collect::<rusqlite::Result<Vec<_>>>()?
            };
            db.conn().execute(
                "DELETE FROM group_tabs
                 WHERE EXISTS (
                     SELECT 1 FROM tab_groups
                     WHERE tab_groups.session_name = group_tabs.session_name
                       AND tab_groups.name = group_tabs.group_name
                       AND substr(tab_groups.name, 1, length(?1)) = ?1
                 )",
                [&prefix],
            )?;
            db.conn().execute(
                "DELETE FROM tab_groups WHERE substr(name, 1, length(?1)) = ?1",
                [&prefix],
            )?;
            Ok(pane_sessions)
        })
    }

    /// Wipe a session's whole persisted layout (groups + tabs). The host
    /// persists snapshots as clear-then-insert inside one transaction so
    /// closed/renamed entries can't linger.
    fn clear_session_layout(&self, session: &str) -> Result<()> {
        self.conn().execute(
            "DELETE FROM group_tabs WHERE session_name=?1",
            params![session],
        )?;
        self.conn().execute(
            "DELETE FROM tab_groups WHERE session_name=?1",
            params![session],
        )?;
        Ok(())
    }

    /// Record which worktree group is active (for restoring focus on resurrect).
    fn set_active_tab(&self, session: &str, tab: &str, now: i64) -> Result<()> {
        self.conn().execute(
            "INSERT INTO session_state (session_name, active_tab, updated_at)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(session_name) DO UPDATE SET active_tab=?2, updated_at=?3",
            params![session, tab, now],
        )?;
        Ok(())
    }

    /// Record the workspace (repo path) that was focused at the last switch.
    /// Stored as a global `ui_state` pointer ("" scope) so startup can reopen
    /// the workspace the user was actually in — independent of the
    /// `workspaces.last_active` column, which also orders the sidebar tree and
    /// must not reshuffle on every switch.
    fn set_active_workspace(&self, repo_path: &str) -> Result<()> {
        self.set_ui_state("", "active_workspace", repo_path)
    }

    /// The workspace recorded by [`Self::set_active_workspace`], if any.
    fn active_workspace(&self) -> Result<Option<String>> {
        self.get_ui_state("", "active_workspace")
    }

    /// The tab that was active at exit, if recorded.
    fn active_tab(&self, session: &str) -> Result<Option<String>> {
        let r = self
            .conn()
            .query_row(
                "SELECT active_tab FROM session_state WHERE session_name=?1",
                params![session],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?;
        Ok(r.flatten())
    }

    /// Read a persisted UI-state value, `None` if unset.
    fn get_ui_state(&self, scope: &str, key: &str) -> Result<Option<String>> {
        let r = self
            .conn()
            .query_row(
                "SELECT value FROM ui_state WHERE scope=?1 AND key=?2",
                params![scope, key],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?;
        Ok(r.flatten())
    }

    /// Upsert a persisted UI-state value for `(scope, key)`.
    fn set_ui_state(&self, scope: &str, key: &str, value: &str) -> Result<()> {
        self.conn().execute(
            "INSERT INTO ui_state (scope, key, value)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(scope, key) DO UPDATE SET value=?3",
            params![scope, key, value],
        )?;
        Ok(())
    }

    /// Delete a persisted UI-state value (e.g. unpinning). No-op if absent.
    fn del_ui_state(&self, scope: &str, key: &str) -> Result<()> {
        self.conn().execute(
            "DELETE FROM ui_state WHERE scope=?1 AND key=?2",
            params![scope, key],
        )?;
        Ok(())
    }

    /// Delete every persisted UI-state key in `scope` starting with `prefix`.
    /// `ESCAPE` guards the LIKE pattern so a prefix containing `%`/`_` (both
    /// legal in slugs/branch names) matches literally.
    fn del_ui_state_prefix(&self, scope: &str, prefix: &str) -> Result<()> {
        let escaped = prefix
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        self.conn().execute(
            "DELETE FROM ui_state WHERE scope=?1 AND key LIKE ?2 ESCAPE '\\'",
            params![scope, format!("{escaped}%")],
        )?;
        Ok(())
    }

    /// All `(key, value)` pairs in a scope — used to load every collapse/pin
    /// entry at once on sidebar build.
    fn ui_state_in_scope(&self, scope: &str) -> Result<Vec<(String, String)>> {
        let mut stmt = self
            .conn()
            .prepare("SELECT key, value FROM ui_state WHERE scope=?1")?;
        let rows = stmt
            .query_map(params![scope], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
            })?
            .filter_map(|r| r.ok())
            .filter_map(|(k, v)| v.map(|v| (k, v)))
            .collect();
        Ok(rows)
    }

    /// Record the running-pin set (an opaque JSON string) for a session without
    /// disturbing `active_tab`. Used by the native host to resurrect pins.
    fn set_pin_state(&self, session: &str, json: &str, now: i64) -> Result<()> {
        self.conn().execute(
            "INSERT INTO session_state (session_name, pin_state, updated_at)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(session_name) DO UPDATE SET pin_state=?2, updated_at=?3",
            params![session, json, now],
        )?;
        Ok(())
    }

    /// The running-pin JSON recorded for a session, if any.
    fn pin_state(&self, session: &str) -> Result<Option<String>> {
        let r = self
            .conn()
            .query_row(
                "SELECT pin_state FROM session_state WHERE session_name=?1",
                params![session],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?;
        Ok(r.flatten())
    }

    // --- command-palette frecency -----------------------------------------
    /// Record that `key` was just chosen (increment count, stamp last_used).
    fn bump_palette_usage(&self, key: &str) -> Result<()> {
        self.conn().execute(
            r#"INSERT INTO palette_usage(key,count,last_used)
               VALUES(?1,1,?2)
               ON CONFLICT(key) DO UPDATE SET count=count+1, last_used=?2"#,
            params![key, util::now()],
        )?;
        Ok(())
    }

    /// All usage rows as (key, count, last_used), for frecency ranking.
    fn palette_usage(&self) -> Result<Vec<(String, i64, i64)>> {
        let mut stmt = self
            .conn()
            .prepare("SELECT key, count, last_used FROM palette_usage")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, Option<i64>>(2)?.unwrap_or(0),
            ))
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    fn terminals(&self) -> Result<Vec<crate::models::TerminalRow>> {
        let mut stmt = self.conn().prepare(
            "SELECT id, name, kind, connection_string, folder_id, created_at, last_active, position,
                    COALESCE(sandbox_backend, ''), COALESCE(env_name, ''),
                    COALESCE(observed_backend, '')
             FROM terminals ORDER BY position, last_active DESC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(crate::models::TerminalRow {
                id: r.get(0)?,
                name: r.get(1)?,
                kind: r.get(2)?,
                connection_string: r.get(3)?,
                folder_id: r.get(4)?,
                created_at: r.get(5)?,
                last_active: r.get(6)?,
                position: r.get(7)?,
                sandbox_backend: r.get(8)?,
                env_name: r.get(9)?,
                observed_backend: r.get(10)?,
            })
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    fn put_terminal(
        &self,
        name: &str,
        kind: &str,
        connection_string: &str,
        folder_id: Option<i64>,
    ) -> Result<i64> {
        let now = util::now();
        self.conn().execute(
            r#"INSERT INTO terminals(name, kind, connection_string, folder_id, created_at, last_active, position)
               VALUES(?1, ?2, ?3, ?4, ?5, ?5, (SELECT COALESCE(MAX(position), -1) + 1 FROM terminals))
               ON CONFLICT(name) DO UPDATE SET
                 kind=?2, connection_string=?3, folder_id=COALESCE(?4, folder_id), last_active=?5"#,
            params![name, kind, connection_string, folder_id, now],
        )?;
        let id: i64 = self.conn().query_row(
            "SELECT id FROM terminals WHERE name=?1",
            params![name],
            |r| r.get(0),
        )?;
        Ok(id)
    }

    fn set_terminal_sandbox(&self, name: &str, backend: &str) -> Result<()> {
        self.conn().execute(
            "UPDATE terminals SET sandbox_backend=?2 WHERE name=?1",
            params![name, backend],
        )?;
        Ok(())
    }

    fn set_terminal_observed(&self, name: &str, backend: &str) -> Result<()> {
        self.conn().execute(
            "UPDATE terminals SET observed_backend=?2 WHERE name=?1",
            params![name, backend],
        )?;
        Ok(())
    }

    fn set_terminal_env(&self, name: &str, env: &str) -> Result<()> {
        self.conn().execute(
            "UPDATE terminals SET env_name=?2 WHERE name=?1",
            params![name, env],
        )?;
        Ok(())
    }

    fn del_terminal(&self, id: i64) -> Result<()> {
        self.conn()
            .execute("DELETE FROM terminals WHERE id = ?1", params![id])?;
        Ok(())
    }

    fn rename_terminal(&self, id: i64, new_name: &str) -> Result<()> {
        self.conn().execute(
            "UPDATE terminals SET name = ?1 WHERE id = ?2",
            params![new_name, id],
        )?;
        Ok(())
    }

    /// Terminal analogue of [`WorkspaceStore::swap_worktree_positions`] (name
    /// key). Read both first, then write — a CASE-UPDATE that reads the table it
    /// mutates can observe its own intermediate write. Heal NULL/tied positions
    /// (older `local` reseeds / pre-reorder rows) before swapping.
    fn swap_terminal_positions(&self, a_name: &str, b_name: &str) -> Result<()> {
        self.transaction(|db| {
            let pos = |name: &str| -> Result<Option<i64>> {
                Ok(db
                    .conn()
                    .query_row(
                        "SELECT position FROM terminals WHERE name=?1",
                        params![name],
                        |r| r.get::<_, Option<i64>>(0),
                    )
                    .optional()?
                    .flatten())
            };
            let (mut pa, mut pb) = (pos(a_name)?, pos(b_name)?);
            if pa.is_none() || pb.is_none() || pa == pb {
                db.normalize_terminal_positions()?;
                (pa, pb) = (pos(a_name)?, pos(b_name)?);
            }
            if let (Some(pa), Some(pb)) = (pa, pb) {
                db.set_terminal_position(a_name, pb)?;
                db.set_terminal_position(b_name, pa)?;
            }
            Ok(())
        })
    }

    fn set_terminal_position(&self, name: &str, position: i64) -> Result<()> {
        self.conn().execute(
            "UPDATE terminals SET position=?2 WHERE name=?1",
            params![name, position],
        )?;
        Ok(())
    }
}

/// Position-healing helpers for the manual-reorder swaps. Kept as inherent
/// `Db` methods (not on the `WorkspaceStore` seam) — they're an implementation
/// detail of `swap_*_positions`, not part of the store contract.
impl Db {
    /// Prune one bounded slice of stale semantic and cache rows.
    ///
    /// This performs filesystem checks and database writes, so callers must
    /// invoke it from background work, never from the UI event loop. The
    /// terminal slice also runs the bounded orphan-cache sweep.
    pub fn prune_stale_cache_slice(&self, after_rowid: i64, limit: usize) -> Result<(usize, i64)> {
        self.prune_missing_semantic_files(after_rowid, limit.clamp(1, 4096))
    }

    /// Drain stale semantic rows and orphan worktree caches to completion.
    ///
    /// This performs filesystem checks and database writes, so callers must
    /// invoke it from background or CLI work, never from the UI event loop.
    /// The semantic count is a net figure and may be skewed by concurrent
    /// writers. A running UI instance may prune concurrently; both paths use
    /// idempotent bounded transactions.
    pub fn prune_stale_caches_to_end(
        &self,
        slice_rows: usize,
        progress: &mut dyn FnMut(usize),
    ) -> Result<(u64, usize)> {
        let before: i64 = self
            .conn()
            .query_row("SELECT COUNT(*) FROM sem_entity", [], |row| row.get(0))?;

        // Count cache sweeps before the semantic walk because its terminal
        // slice also runs one bounded orphan sweep and does not report its
        // count. If this cap is reached, that terminal slice may remove up to
        // one additional budget of cache keys that is not included here.
        let mut removed_cache_keys = 0;
        for _ in 0..65 {
            let removed = self.prune_orphan_worktree_caches(ORPHAN_CACHE_SWEEP_MAX_KEYS)?;
            removed_cache_keys += removed;
            if removed < ORPHAN_CACHE_SWEEP_MAX_KEYS {
                break;
            }
        }

        let limit = slice_rows.clamp(1, 4096);
        let mut after_rowid = 0;
        let mut total_scanned = 0;
        loop {
            let (scanned, next_rowid) = self.prune_missing_semantic_files(after_rowid, limit)?;
            if scanned == 0 {
                break;
            }
            after_rowid = next_rowid;
            total_scanned += scanned;
            progress(total_scanned);
        }
        let after: i64 = self
            .conn()
            .query_row("SELECT COUNT(*) FROM sem_entity", [], |row| row.get(0))?;
        Ok((
            u64::try_from(before.saturating_sub(after)).unwrap_or(0),
            removed_cache_keys,
        ))
    }

    /// Renumber every workspace to a contiguous, distinct `position` (0..n) in
    /// its current display order, healing NULL/duplicate positions (from insert
    /// paths that bypass `put_workspace`, or backfill ties). Uses the same
    /// `ORDER BY` as [`WorkspaceStore::workspaces`] so the visible order is
    /// preserved; `repo_path` is the final deterministic tie-break. Idempotent.
    pub(crate) fn normalize_workspace_positions(&self) -> Result<()> {
        let paths: Vec<String> = {
            let mut stmt = self.conn().prepare(
                "SELECT repo_path FROM workspaces
                 ORDER BY position, last_active DESC, repo_path",
            )?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            rows.filter_map(|r| r.ok()).collect()
        };
        for (i, p) in paths.iter().enumerate() {
            self.conn().execute(
                "UPDATE workspaces SET position=?2 WHERE repo_path=?1",
                params![p, i as i64],
            )?;
        }
        Ok(())
    }

    /// Worktree analogue of [`Db::normalize_workspace_positions`]. Uses the same
    /// `ORDER BY` as [`WorkspaceStore::worktrees`] so filed/loose worktree order
    /// is preserved while positions become contiguous + distinct.
    pub(crate) fn normalize_worktree_positions(&self) -> Result<()> {
        let paths: Vec<String> = {
            let mut stmt = self.conn().prepare(
                "SELECT worktree FROM worktrees ORDER BY position, created_at, worktree",
            )?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            rows.filter_map(|r| r.ok()).collect()
        };
        for (i, p) in paths.iter().enumerate() {
            self.conn().execute(
                "UPDATE worktrees SET position=?2 WHERE worktree=?1",
                params![p, i as i64],
            )?;
        }
        Ok(())
    }

    /// Persist the last OSC window title observed for a worktree. Keyed by
    /// worktree path (the same key the sidebar's `sidebar_window_titles` map
    /// uses), so the title survives workspace parking, cold resurrects, and
    /// restarts. Written on change only (see the loop's live-title merge), not
    /// per frame. Not on the `WorkspaceStore` seam — a compositor-side cache.
    pub fn set_worktree_window_title(&self, worktree: &str, title: &str) -> Result<()> {
        self.conn().execute(
            "INSERT INTO worktree_titles (worktree, last_title, updated_at)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(worktree) DO UPDATE SET last_title=?2, updated_at=?3",
            params![worktree, title, util::now()],
        )?;
        Ok(())
    }

    /// Every stored worktree title as a `path -> title` map, for seeding the
    /// sidebar on the first frame so persisted titles show before any pane
    /// re-emits one. Skips empty/NULL titles.
    pub fn all_worktree_titles(&self) -> Result<std::collections::BTreeMap<String, String>> {
        let mut stmt = self.conn().prepare(
            "SELECT worktree, last_title FROM worktree_titles
             WHERE last_title IS NOT NULL AND last_title <> ''",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// Terminal analogue of [`Db::normalize_worktree_positions`]. Uses the same
    /// `ORDER BY` as [`WorkspaceStore::terminals`] (`position, last_active DESC`)
    /// so the visible order is preserved while positions become contiguous +
    /// distinct; `name` is the final deterministic tie-break. Idempotent.
    pub(crate) fn normalize_terminal_positions(&self) -> Result<()> {
        let names: Vec<String> = {
            let mut stmt = self
                .conn()
                .prepare("SELECT name FROM terminals ORDER BY position, last_active DESC, name")?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            rows.filter_map(|r| r.ok()).collect()
        };
        for (i, n) in names.iter().enumerate() {
            self.conn().execute(
                "UPDATE terminals SET position=?2 WHERE name=?1",
                params![n, i as i64],
            )?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::ORPHAN_CACHE_SWEEP_MAX_KEYS;

    #[test]
    fn prune_stale_caches_to_end_removes_orphans_and_counts() {
        use crate::store::{CacheStore, SemanticStore, WorkspaceStore};

        let db = Db::open_memory().unwrap();
        let remote = r#"{"path":"/remote/repo/wt","host":"builder"}"#;
        db.put_worktree(
            "tab",
            "/remote/repo",
            "/remote/repo/wt",
            "remote",
            Some(remote),
            None,
        )
        .unwrap();
        let gone_entities = (0..3)
            .map(|index| crate::store::SemEntityRow {
                id: format!("gone-{index}"),
                file: "/gone/wt/a.rs".into(),
                name: format!("gone-{index}"),
                kind: crate::semantic::EntityKind::Function,
                start_line: index + 1,
                end_line: index + 1,
                source_hash: "hash".into(),
            })
            .collect::<Vec<_>>();
        db.replace_file_entities("/gone/wt/a.rs", &gone_entities)
            .unwrap();
        db.replace_file_entities(
            "/remote/repo/wt/a.rs",
            &[crate::store::SemEntityRow {
                id: "remote-entity".into(),
                file: "/remote/repo/wt/a.rs".into(),
                name: "remote".into(),
                kind: crate::semantic::EntityKind::Function,
                start_line: 1,
                end_line: 1,
                source_hash: "hash".into(),
            }],
        )
        .unwrap();
        db.put_pr_cache("/gone/wt", "main", "[]").unwrap();

        let mut progress = Vec::new();
        let removed = db
            .prune_stale_caches_to_end(1, &mut |scanned| progress.push(scanned))
            .unwrap();

        assert_eq!(removed, (3, 1));
        assert!(db.entities_under("/gone/wt").unwrap().is_empty());
        assert_eq!(db.entities_under("/remote/repo/wt").unwrap().len(), 1);
        assert!(progress.len() >= 3);
        assert!(progress.windows(2).all(|counts| counts[0] <= counts[1]));
    }

    #[test]
    fn prune_stale_caches_to_end_drains_more_than_one_key_budget() {
        let db = Db::open_memory().unwrap();
        db.put_worktree("tab", "/live", "/live", "main", None, None)
            .unwrap();
        let tx = db.conn().unchecked_transaction().unwrap();
        for index in 0..(ORPHAN_CACHE_SWEEP_MAX_KEYS + 2) {
            tx.execute(
                "INSERT INTO worktree_titles(worktree,last_title,updated_at) VALUES(?1,'title',1)",
                [format!("/gone/k{index}")],
            )
            .unwrap();
        }
        tx.execute(
            "INSERT INTO worktree_titles(worktree,last_title,updated_at) VALUES('/live','title',1)",
            [],
        )
        .unwrap();
        tx.commit().unwrap();

        let removed = db.prune_stale_caches_to_end(1, &mut |_| {}).unwrap();

        assert_eq!(removed.1, ORPHAN_CACHE_SWEEP_MAX_KEYS + 2);
        assert!(db.all_worktree_titles().unwrap().contains_key("/live"));
    }

    #[test]
    fn prune_stale_caches_to_end_caps_extra_cache_sweeps() {
        let db = Db::open_memory().unwrap();
        let tx = db.conn().unchecked_transaction().unwrap();
        let total_keys = ORPHAN_CACHE_SWEEP_MAX_KEYS * 66 + 1;
        for index in 0..total_keys {
            tx.execute(
                "INSERT INTO worktree_titles(worktree,last_title,updated_at) VALUES(?1,'title',1)",
                [format!("/gone/k{index:05}")],
            )
            .unwrap();
        }
        tx.commit().unwrap();

        let removed = db.prune_stale_caches_to_end(1, &mut |_| {}).unwrap();

        assert_eq!(removed.1, ORPHAN_CACHE_SWEEP_MAX_KEYS * 65);
        assert_eq!(db.all_worktree_titles().unwrap().len(), 1);
    }

    #[test]
    fn worktree_record_preserves_absence_nullable_fields_and_decode_errors() {
        let db = Db::open_memory().unwrap();
        let store: &dyn WorkspaceStore = &db;
        assert!(store.worktree_record("private/missing").unwrap().is_none());
        db.conn()
            .execute(
                "INSERT INTO worktrees(worktree, location, env_name) VALUES(?1, ?2, ?3)",
                ["private/remote", "remote-placement", "  "],
            )
            .unwrap();
        let row = store.worktree_record("private/remote").unwrap().unwrap();
        assert_eq!(row.worktree, "private/remote");
        assert_eq!(row.location, "remote-placement");
        assert_eq!(row.position, 0);
        assert!(row.branch.is_empty() && row.repo_root.is_empty());
        assert!(row.env_name.is_none());
        db.conn()
            .execute(
                "UPDATE worktrees SET position='malformed' WHERE worktree=?1",
                ["private/remote"],
            )
            .unwrap();
        assert!(store.worktree_record("private/remote").is_err());
        assert!(store.worktree_record("private/missing").unwrap().is_none());
        assert!(
            store.worktrees().unwrap().is_empty(),
            "legacy list remains lossy"
        );
    }

    #[test]
    fn worktree_record_query_failure_is_not_absence() {
        let db = Db::open_memory().unwrap();
        let store: &dyn WorkspaceStore = &db;
        db.conn().execute_batch("DROP TABLE worktrees").unwrap();
        assert!(store.worktree_record("private/missing").is_err());
    }

    #[test]
    fn delete_tab_groups_for_worktree_all_sessions_keys_on_path() {
        use crate::models::{GroupTabRow, TabGroupRow};
        let db = Db::open_memory().unwrap();
        let group = |name: &str, worktree: &str| TabGroupRow {
            name: name.into(),
            kind: "branch".into(),
            worktree: worktree.into(),
            ordinal: 0,
            active_tab: 0,
        };
        let tab = |group_name: &str| GroupTabRow {
            group_name: group_name.into(),
            ordinal: 0,
            title: "1".into(),
            pane_tree: r#"{"leaf":0}"#.into(),
            focused_pane: 0,
            pane_cwds: String::new(),
            pane_cmds: String::new(),
            pane_sessions: String::new(),
            scrollback_snapshot: String::new(),
        };
        db.put_tab_group("session-a", &group("app/a", "/wt/feature"))
            .unwrap();
        db.put_group_tab("session-a", &tab("app/a")).unwrap();
        db.put_tab_group("session-b", &group("other/feature", "/wt/feature"))
            .unwrap();
        db.put_group_tab("session-b", &tab("other/feature"))
            .unwrap();
        db.put_tab_group("session-a", &group("app/other", "/wt/other"))
            .unwrap();
        db.put_group_tab("session-a", &tab("app/other")).unwrap();

        db.delete_tab_groups_for_worktree_all_sessions("/wt/feature")
            .unwrap();

        assert_eq!(db.groups_for_session("session-a").unwrap().len(), 1);
        assert_eq!(
            db.groups_for_session("session-a").unwrap()[0].name,
            "app/other"
        );
        assert!(db.groups_for_session("session-b").unwrap().is_empty());
        assert_eq!(
            db.group_tabs_for_session("session-a").unwrap()[0].group_name,
            "app/other"
        );
        assert!(db.group_tabs_for_session("session-b").unwrap().is_empty());
    }

    #[test]
    fn delete_tab_groups_with_prefix_cleans_every_session_and_child_tabs() {
        use crate::models::{GroupTabRow, TabGroupRow};
        let db = Db::open_memory().unwrap();
        let group = |name: &str| TabGroupRow {
            name: name.into(),
            kind: "branch".into(),
            worktree: format!("/wt/{name}"),
            ordinal: 0,
            active_tab: 0,
        };
        let tab = |group_name: &str| GroupTabRow {
            group_name: group_name.into(),
            ordinal: 0,
            title: "1".into(),
            pane_tree: r#"{"leaf":0}"#.into(),
            focused_pane: 0,
            pane_cwds: String::new(),
            pane_cmds: String::new(),
            pane_sessions: String::new(),
            scrollback_snapshot: String::new(),
        };
        for (session, name) in [
            ("app-session", "lib/home"),
            ("other-session", "lib/feat"),
            ("other-session", "library/feat"),
        ] {
            db.put_tab_group(session, &group(name)).unwrap();
            db.put_group_tab(session, &tab(name)).unwrap();
        }

        assert!(db.delete_tab_groups_with_prefix("lib").unwrap().is_empty());

        assert!(db.groups_for_session("app-session").unwrap().is_empty());
        let remaining = db.groups_for_session("other-session").unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].name, "library/feat");
        assert_eq!(db.group_tabs_for_session("app-session").unwrap().len(), 0);
        let remaining_tabs = db.group_tabs_for_session("other-session").unwrap();
        assert_eq!(remaining_tabs.len(), 1);
        assert_eq!(remaining_tabs[0].group_name, "library/feat");
    }

    #[test]
    fn delete_tab_groups_with_prefix_returns_deleted_pane_sessions() {
        use crate::models::{GroupTabRow, TabGroupRow};
        let db = Db::open_memory().unwrap();
        let group = |name: &str| TabGroupRow {
            name: name.into(),
            kind: "branch".into(),
            worktree: format!("/wt/{name}"),
            ordinal: 0,
            active_tab: 0,
        };
        let tab = |group_name: &str, pane_sessions: &str| GroupTabRow {
            group_name: group_name.into(),
            ordinal: 0,
            title: "1".into(),
            pane_tree: r#"{"leaf":0}"#.into(),
            focused_pane: 0,
            pane_cwds: String::new(),
            pane_cmds: String::new(),
            pane_sessions: pane_sessions.into(),
            scrollback_snapshot: String::new(),
        };
        db.put_tab_group("other-session", &group("lib/feat"))
            .unwrap();
        db.put_group_tab(
            "other-session",
            &tab(
                "lib/feat",
                r#"{"0":{"provider":"daemon","id":"x","session":"sid-a"}}"#,
            ),
        )
        .unwrap();
        db.put_tab_group("other-session", &group("library/feat"))
            .unwrap();
        db.put_group_tab(
            "other-session",
            &tab(
                "library/feat",
                r#"{"0":{"provider":"daemon","id":"y","session":"sid-b"}}"#,
            ),
        )
        .unwrap();

        let deleted = db.delete_tab_groups_with_prefix("lib").unwrap();

        assert_eq!(
            deleted,
            vec![(
                "other-session".to_string(),
                r#"{"0":{"provider":"daemon","id":"x","session":"sid-a"}}"#.to_string(),
            )]
        );
        assert!(
            db.groups_for_session("other-session")
                .unwrap()
                .iter()
                .all(|group| group.name == "library/feat")
        );
        assert!(
            db.group_tabs_for_session("other-session")
                .unwrap()
                .iter()
                .all(|tab| tab.group_name == "library/feat")
        );
    }

    #[test]
    fn delete_tab_groups_for_worktree_all_sessions_rolls_back_with_bookkeeping() {
        use crate::models::{GroupTabRow, TabGroupRow};
        let db = Db::open_memory().unwrap();
        let group = TabGroupRow {
            name: "app/feature".into(),
            kind: "branch".into(),
            worktree: "/wt/feature".into(),
            ordinal: 0,
            active_tab: 0,
        };
        let tab = GroupTabRow {
            group_name: "app/feature".into(),
            ordinal: 0,
            title: "1".into(),
            pane_tree: r#"{"leaf":0}"#.into(),
            focused_pane: 0,
            pane_cwds: String::new(),
            pane_cmds: String::new(),
            pane_sessions: String::new(),
            scrollback_snapshot: String::new(),
        };
        db.put_tab_group("session-a", &group).unwrap();
        db.put_group_tab("session-a", &tab).unwrap();

        let result: anyhow::Result<()> = db.transaction(|db| {
            db.delete_tab_groups_for_worktree_all_sessions("/wt/feature")?;
            anyhow::bail!("simulate queue bookkeeping failure")
        });
        assert!(result.is_err());
        assert_eq!(db.groups_for_session("session-a").unwrap(), [group]);
        assert_eq!(db.group_tabs_for_session("session-a").unwrap(), [tab]);
    }
    use crate::db::Db;
    use crate::store::WorkspaceStore;
    use rusqlite::params;

    #[test]
    fn env_pin_upserts_before_registration_and_survives_it() {
        let db = Db::open_memory().unwrap();
        // Pin an env on a worktree the DB has never seen (fresh `git worktree
        // add`, not yet opened): must insert a row, not silently no-op.
        db.set_worktree_env("/wt/new", "machine0").unwrap();
        assert_eq!(
            db.worktree_env("/wt/new").unwrap().as_deref(),
            Some("machine0")
        );
        assert_eq!(
            db.effective_env("/wt/new", "/repo").as_deref(),
            Some("machine0")
        );
        // First open registers the full row; the pin must survive the upsert.
        db.put_worktree("tab", "/repo", "/wt/new", "feat", None, None)
            .unwrap();
        assert_eq!(
            db.worktree_env("/wt/new").unwrap().as_deref(),
            Some("machine0")
        );
        // `""` clears back to inherit.
        db.set_worktree_env("/wt/new", "").unwrap();
        assert_eq!(db.worktree_env("/wt/new").unwrap(), None);
    }

    #[test]
    fn repro_env_pin_then_register_row_visible_in_worktrees() {
        let db = Db::open_memory().unwrap();
        db.set_worktree_env("/wt/new", "machine0").unwrap();
        db.put_worktree("tab", "/repo", "/wt/new", "feat", None, None)
            .unwrap();
        let rows = db.worktrees().unwrap();
        assert_eq!(rows.len(), 1, "row dropped from worktrees(): {rows:?}");
        // put_worktree must heal the NULL columns the env-pin insert left behind,
        // and the pin itself must survive registration.
        let row = &rows[0];
        assert_eq!(row.branch, "feat");
        assert_eq!(row.repo_root, "/repo");
        assert!(row.created_at > 0, "created_at not healed: {row:?}");
        assert_eq!(row.env_name.as_deref(), Some("machine0"));
    }

    #[test]
    fn env_pin_only_row_survives_worktrees_scan() {
        // A pin set on a worktree that has not been opened yet (no put_worktree)
        // must still be enumerable, not silently dropped by NULL columns.
        let db = Db::open_memory().unwrap();
        db.set_worktree_env("/wt/pending", "vps").unwrap();
        let rows = db.worktrees().unwrap();
        assert_eq!(rows.len(), 1, "pin-only row dropped: {rows:?}");
        assert_eq!(rows[0].env_name.as_deref(), Some("vps"));
        assert_eq!(rows[0].branch, "");
    }

    #[test]
    fn workspace_tombstone_round_trips_and_defaults_absent() {
        let db = Db::open_memory().unwrap();
        // Absent by default — a never-removed workspace isn't tombstoned.
        assert!(!db.workspace_tombstoned("/repo/app").unwrap());
        // Removing sets the tombstone; unrelated paths stay untombstoned.
        db.tombstone_workspace("/repo/app").unwrap();
        assert!(db.workspace_tombstoned("/repo/app").unwrap());
        assert!(!db.workspace_tombstoned("/repo/svc").unwrap());
        // An explicit reopen clears exactly that path's tombstone.
        db.clear_workspace_tombstone("/repo/app").unwrap();
        assert!(!db.workspace_tombstoned("/repo/app").unwrap());
        // Clearing an absent tombstone is a no-op, not an error.
        db.clear_workspace_tombstone("/repo/none").unwrap();
    }

    #[test]
    fn set_worktree_location_rebinds_and_noops_on_unknown() {
        let db = Db::open_memory().unwrap();
        db.put_worktree("tab", "/repo", "/wt/r", "feat", Some("old-blob"), None)
            .unwrap();
        db.set_worktree_location("/wt/r", "new-blob").unwrap();
        assert_eq!(
            db.location_for("/wt/r").unwrap().as_deref(),
            Some("new-blob")
        );
        // Unknown path: Ok no-op — no row materializes.
        db.set_worktree_location("/wt/ghost", "blob").unwrap();
        assert_eq!(db.location_for("/wt/ghost").unwrap(), None);
    }

    #[test]
    fn reregister_without_location_keeps_the_persisted_one() {
        let db = Db::open_memory().unwrap();
        let loc = r#"{"control_prefix":["sprite","exec"],"path":"/workspace"}"#;
        db.put_worktree("tab", "/repo", "/wt/p", "feat", Some(loc), None)
            .unwrap();
        // Re-register callers (fold-into-folder, tracker) pass None for the
        // location they don't know — the persisted provider blob must survive.
        db.put_worktree("tab", "/repo", "/wt/p", "feat", None, None)
            .unwrap();
        assert_eq!(db.location_for("/wt/p").unwrap().as_deref(), Some(loc));
        // An explicit new location still replaces it.
        db.put_worktree("tab", "/repo", "/wt/p", "feat", Some("local"), None)
            .unwrap();
        assert_eq!(db.location_for("/wt/p").unwrap().as_deref(), Some("local"));
    }

    #[test]
    fn del_worktree_cascades_to_disk_cache() {
        use crate::store::WorktreeAuxStore;
        let db = Db::open_memory().unwrap();
        // A registered worktree with a cached size, plus a sibling.
        db.put_worktree("tab", "/repo", "/wt/a", "feat", None, None)
            .unwrap();
        db.put_worktree("tab", "/repo", "/wt/b", "fix", None, None)
            .unwrap();
        db.put_worktree_disk("/wt/a", 5_000, 4_200).unwrap();
        db.put_worktree_disk("/wt/b", 1_000, 900).unwrap();

        // Removing the worktree drops its size row — no orphan left behind.
        db.del_worktree("/wt/a").unwrap();
        assert!(db.get_worktree_disk("/wt/a").unwrap().is_none());
        assert_eq!(db.all_worktree_disk().unwrap().len(), 1);
        assert_eq!(
            db.all_worktree_disk().unwrap().get("/wt/b"),
            Some(&(1_000, 900))
        );

        // Per-repo removal cascades for every owned worktree too.
        db.del_worktrees_for_repo("/repo").unwrap();
        assert!(db.all_worktree_disk().unwrap().is_empty());
    }

    #[test]
    fn del_worktree_cascades_to_semantic_entities_and_path_caches() {
        use crate::semantic::EntityKind;
        use crate::store::SemanticStore;

        let db = Db::open_memory().unwrap();
        db.put_worktree("tab", "/repo", "/wt/a", "feat", None, None)
            .unwrap();
        db.put_worktree("tab", "/repo", "/wt/a2", "sibling", None, None)
            .unwrap();
        let entity = |id: &str, file: &str| crate::store::SemEntityRow {
            id: id.to_string(),
            file: file.to_string(),
            name: "f".to_string(),
            kind: EntityKind::Function,
            start_line: 1,
            end_line: 1,
            source_hash: "hash".to_string(),
        };
        db.upsert_entity(&entity("a", "/wt/a/src/a.rs")).unwrap();
        db.upsert_entity(&entity("sibling", "/wt/a2/src/b.rs"))
            .unwrap();
        db.conn()
            .execute(
                "INSERT INTO sem_edge(src_id,dst_id,kind) VALUES('sibling','a','call')",
                [],
            )
            .unwrap();
        db.conn()
            .execute_batch(
                "INSERT INTO commit_cache(worktree,json,fetched_at) VALUES('/wt/a','[]',1);
                 INSERT INTO worktree_titles(worktree,last_title,updated_at) VALUES('/wt/a','title',1);
                 INSERT INTO issue_links(worktree_path,issue_id,linked_at) VALUES('/wt/a','i-1',1);
                 INSERT INTO notifications(kind,issue_id,message,created_at_ms,read,worktree_path)
                   VALUES('test','i-1','message',1,0,'/wt/a');",
            )
            .unwrap();
        db.put_glyph_cache("/wt/a", "{}").unwrap();
        db.put_glyph_cache("/wt/a2", "{}").unwrap();

        db.del_worktree("/wt/a").unwrap();

        assert_eq!(db.entities_under("/wt/a").unwrap().len(), 0);
        assert_eq!(db.entities_under("/wt/a2").unwrap().len(), 1);
        let edge_count: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM sem_edge", [], |row| row.get(0))
            .unwrap();
        assert_eq!(edge_count, 0, "edges to removed entities must be deleted");
        for (table, key) in [
            ("commit_cache", "worktree"),
            ("worktree_titles", "worktree"),
        ] {
            let sql = format!("SELECT COUNT(*) FROM {table} WHERE {key}='/wt/a'");
            let count: i64 = db.conn().query_row(&sql, [], |row| row.get(0)).unwrap();
            assert_eq!(count, 0, "{table} retained the removed worktree");
        }
        for (table, key) in [
            ("issue_links", "worktree_path"),
            ("notifications", "worktree_path"),
        ] {
            let sql = format!("SELECT COUNT(*) FROM {table} WHERE {key}='/wt/a'");
            let count: i64 = db.conn().query_row(&sql, [], |row| row.get(0)).unwrap();
            assert_eq!(count, 1, "{table} should survive closing the worktree");
        }
        assert_eq!(db.all_glyph_cache().len(), 1);
        assert_eq!(db.all_glyph_cache()[0].0, "/wt/a2");
    }

    #[test]
    fn orphan_cache_sweep_prunes_only_missing_unregistered_worktrees() {
        use crate::store::WorkspaceStore;

        let db = Db::open_memory().unwrap();
        let existing = tempfile::tempdir().unwrap();
        let existing_path = existing.path().to_string_lossy().into_owned();
        let remote = r#"{"path":"/remote/wt","host":"builder"}"#;
        db.put_worktree("tab", "/repo", "/live", "main", None, None)
            .unwrap();
        db.put_worktree("tab", "/repo", "/remote/wt", "remote", Some(remote), None)
            .unwrap();
        db.put_worktree("tab", "/repo", "/gone/wt2", "sibling", None, None)
            .unwrap();

        let keys = [
            "/gone/wt",
            "/live",
            "/remote/wt",
            existing_path.as_str(),
            "/gone/wt2",
        ];
        for key in keys {
            db.conn()
                .execute(
                    "INSERT INTO pr_cache(worktree,branch,json,fetched_at) VALUES(?1,'main','[]',1)",
                    params![key],
                )
                .unwrap();
            db.conn()
                .execute(
                    "INSERT INTO commit_cache(worktree,json,fetched_at) VALUES(?1,'[]',1)",
                    params![key],
                )
                .unwrap();
            db.conn()
                .execute(
                    "INSERT INTO ci_runs_cache(worktree,branch,json,fetched_at) VALUES(?1,'main','[]',1)",
                    params![key],
                )
                .unwrap();
            db.conn()
                .execute(
                    "INSERT INTO worktree_titles(worktree,last_title,updated_at) VALUES(?1,'title',1)",
                    params![key],
                )
                .unwrap();
            db.put_glyph_cache(key, "{}").unwrap();
        }

        assert_eq!(
            db.prune_orphan_worktree_caches(ORPHAN_CACHE_SWEEP_MAX_KEYS)
                .unwrap(),
            1
        );
        for table in [
            "pr_cache",
            "commit_cache",
            "ci_runs_cache",
            "worktree_titles",
        ] {
            let stale_count: i64 = db
                .conn()
                .query_row(
                    &format!("SELECT COUNT(*) FROM {table} WHERE worktree='/gone/wt'"),
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(stale_count, 0, "{table} retained an orphan cache row");
            let retained_count: i64 = db
                .conn()
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(retained_count, 4, "{table} lost a live or existing key");
        }
        let glyphs: std::collections::HashSet<String> = db
            .all_glyph_cache()
            .into_iter()
            .map(|(key, _)| key)
            .collect();
        assert!(!glyphs.contains("/gone/wt"));
        for key in ["/live", "/remote/wt", existing_path.as_str(), "/gone/wt2"] {
            assert!(glyphs.contains(key), "registered/existing {key} was pruned");
        }
    }

    #[test]
    fn orphan_cache_sweep_prunes_issue_cache_for_missing_unregistered_repos() {
        use crate::store::{CacheStore, WorkspaceStore};

        let db = Db::open_memory().unwrap();
        let existing = tempfile::tempdir().unwrap();
        let existing_path = existing.path().to_string_lossy().into_owned();
        db.put_worktree("tab", "/repo", "/live", "main", None, None)
            .unwrap();
        for repo_root in ["/gone/repo", "/repo", "/live", existing_path.as_str()] {
            db.put_issue_cache(repo_root, "linear", "default", "{}")
                .unwrap();
        }

        db.prune_orphan_worktree_caches(ORPHAN_CACHE_SWEEP_MAX_KEYS)
            .unwrap();

        assert!(db.get_all_issue_cache("/gone/repo").unwrap().is_empty());
        for repo_root in ["/repo", "/live", existing_path.as_str()] {
            assert_eq!(
                db.get_all_issue_cache(repo_root).unwrap().len(),
                1,
                "issue cache for registered or existing repo {repo_root} was pruned"
            );
        }
    }

    #[test]
    fn orphan_cache_sweep_bounds_issue_cache_keys() {
        use crate::store::CacheStore;

        let db = Db::open_memory().unwrap();
        for repo_root in ["/gone/repo-a", "/gone/repo-b"] {
            db.put_issue_cache(repo_root, "linear", "default", "{}")
                .unwrap();
        }

        assert_eq!(db.prune_orphan_worktree_caches(1).unwrap(), 0);
        assert_eq!(
            db.get_all_issue_cache("/gone/repo-a").unwrap().len()
                + db.get_all_issue_cache("/gone/repo-b").unwrap().len(),
            1,
            "one issue-cache root must remain after a one-key sweep"
        );
    }

    #[test]
    fn orphan_cache_sweep_prunes_diff_cache_and_undo_marks() {
        use crate::store::{CacheStore, WorkspaceStore, WorktreeAuxStore};

        let db = Db::open_memory().unwrap();
        let existing = tempfile::tempdir().unwrap();
        let existing_path = existing.path().to_string_lossy().into_owned();
        let remote = r#"{"path":"/remote/wt","host":"builder"}"#;
        db.put_worktree("tab", "/repo", "/live", "main", None, None)
            .unwrap();
        db.put_worktree("tab", "/repo", "/remote/wt", "remote", Some(remote), None)
            .unwrap();
        for key in ["/gone/wt", "/live", "/remote/wt", existing_path.as_str()] {
            db.put_diff_cache(key, "[]").unwrap();
            db.add_undo_mark(key, "sha").unwrap();
        }

        db.prune_orphan_worktree_caches(ORPHAN_CACHE_SWEEP_MAX_KEYS)
            .unwrap();

        assert!(db.get_diff_cache("/gone/wt").unwrap().is_none());
        for key in ["/live", "/remote/wt", existing_path.as_str()] {
            assert!(db.get_diff_cache(key).unwrap().is_some());
        }
        let gone_marks: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM undo_marks WHERE worktree='/gone/wt'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(gone_marks, 0);
        for key in ["/live", "/remote/wt", existing_path.as_str()] {
            let count: i64 = db
                .conn()
                .query_row(
                    "SELECT COUNT(*) FROM undo_marks WHERE worktree=?1",
                    params![key],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(count, 1, "undo marks for protected {key} were pruned");
        }
    }

    #[test]
    fn orphan_cache_sweep_prunes_repo_keyed_caches() {
        use crate::store::{CacheStore, WorkspaceStore};

        let db = Db::open_memory().unwrap();
        let existing = tempfile::tempdir().unwrap();
        let existing_path = existing.path().to_string_lossy().into_owned();
        db.put_worktree("tab", "/repo", "/live", "main", None, None)
            .unwrap();
        for repo_root in ["/gone/repo", "/repo", existing_path.as_str()] {
            db.put_pr_branch_cache(repo_root, "[]").unwrap();
            db.put_issue_cache(repo_root, "linear", "default", "{}")
                .unwrap();
            db.conn()
                .execute(
                    "INSERT INTO issue_projects(repo_root,provider,account,json,fetched_at) VALUES(?1,'linear','default','{}',1)",
                    params![repo_root],
                )
                .unwrap();
        }

        assert_eq!(
            db.prune_orphan_worktree_caches(ORPHAN_CACHE_SWEEP_MAX_KEYS)
                .unwrap(),
            0
        );
        for table in ["pr_branch_cache", "issue_cache", "issue_projects"] {
            let stale_count: i64 = db
                .conn()
                .query_row(
                    &format!("SELECT COUNT(*) FROM {table} WHERE repo_root='/gone/repo'"),
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(stale_count, 0, "{table} retained a stale repo cache");
            let protected_count: i64 = db
                .conn()
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(
                protected_count, 2,
                "{table} lost a registered or existing root"
            );
        }

        for repo_root in ["/gone/repo-a", "/gone/repo-b"] {
            db.put_pr_branch_cache(repo_root, "[]").unwrap();
            db.conn()
                .execute(
                    "INSERT INTO issue_projects(repo_root,provider,account,json,fetched_at) VALUES(?1,'linear','default','{}',1)",
                    params![repo_root],
                )
                .unwrap();
        }
        assert_eq!(db.prune_orphan_worktree_caches(1).unwrap(), 0);
        let remaining: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(DISTINCT repo_root) FROM (
                   SELECT repo_root FROM pr_branch_cache WHERE repo_root LIKE '/gone/repo-%'
                   UNION ALL
                   SELECT repo_root FROM issue_projects WHERE repo_root LIKE '/gone/repo-%'
                 )",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            remaining, 1,
            "one repo-root key must remain under a one-key budget"
        );
    }

    #[test]
    fn delete_worktree_cache_rows_covers_diff_cache_and_undo_marks() {
        use crate::store::{CacheStore, WorkspaceStore, WorktreeAuxStore};

        let db = Db::open_memory().unwrap();
        db.put_worktree("tab", "/repo", "/wt", "main", None, None)
            .unwrap();
        db.put_diff_cache("/wt", "[]").unwrap();
        db.add_undo_mark("/wt", "sha").unwrap();

        db.del_worktree("/wt").unwrap();

        assert!(db.get_diff_cache("/wt").unwrap().is_none());
        let marks: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM undo_marks WHERE worktree='/wt'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(marks, 0);
    }

    #[test]
    fn prune_stale_cache_slice_walks_to_terminal() {
        use crate::store::{CacheStore, SemanticStore, WorkspaceStore};

        let db = Db::open_memory().unwrap();
        let remote = r#"{"path":"/remote/repo/wt","host":"builder"}"#;
        db.put_worktree(
            "tab",
            "/remote/repo",
            "/remote/repo/wt",
            "remote",
            Some(remote),
            None,
        )
        .unwrap();
        db.replace_file_entities(
            "/gone/wt/a.rs",
            &[crate::store::SemEntityRow {
                id: "gone-entity".into(),
                file: "/gone/wt/a.rs".into(),
                name: "gone".into(),
                kind: crate::semantic::EntityKind::Function,
                start_line: 1,
                end_line: 1,
                source_hash: "hash".into(),
            }],
        )
        .unwrap();
        db.replace_file_entities(
            "/remote/repo/wt/a.rs",
            &[crate::store::SemEntityRow {
                id: "remote-entity".into(),
                file: "/remote/repo/wt/a.rs".into(),
                name: "remote".into(),
                kind: crate::semantic::EntityKind::Function,
                start_line: 1,
                end_line: 1,
                source_hash: "hash".into(),
            }],
        )
        .unwrap();
        db.put_pr_cache("/gone/wt", "main", "[]").unwrap();

        let mut after_rowid = 0;
        loop {
            let (scanned, next_rowid) = db.prune_stale_cache_slice(after_rowid, 1).unwrap();
            after_rowid = next_rowid;
            if scanned == 0 {
                break;
            }
        }

        assert!(db.entities_under("/gone/wt").unwrap().is_empty());
        assert_eq!(db.entities_under("/remote/repo/wt").unwrap().len(), 1);
        assert!(db.get_pr_cache("/gone/wt").unwrap().is_none());
    }

    #[test]
    fn orphan_cache_sweep_respects_max_keys() {
        let db = Db::open_memory().unwrap();
        for key in ["/gone/wt-a", "/gone/wt-b", "/gone/wt-c"] {
            db.conn()
                .execute(
                    "INSERT INTO pr_cache(worktree,branch,json,fetched_at) VALUES(?1,'main','[]',1)",
                    params![key],
                )
                .unwrap();
        }

        assert_eq!(db.prune_orphan_worktree_caches(2).unwrap(), 2);
        assert_eq!(db.prune_orphan_worktree_caches(2).unwrap(), 1);
        let remaining: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM pr_cache", [], |row| row.get(0))
            .unwrap();
        assert_eq!(remaining, 0);
    }

    #[test]
    fn orphan_cache_sweep_preserves_repo_ancestor_of_remote_worktree_semantics() {
        let db = Db::open_memory().unwrap();
        let remote = r#"{"path":"/remote/repo/.worktrees/x","host":"builder"}"#;
        db.put_worktree(
            "tab",
            "/remote/repo",
            "/remote/repo/.worktrees/x",
            "feature",
            Some(remote),
            None,
        )
        .unwrap();
        db.conn()
            .execute(
                "INSERT INTO workspaces(repo_path,name) VALUES('/remote/standalone','remote')",
                [],
            )
            .unwrap();
        db.conn()
            .execute(
                "INSERT INTO loc_cache(worktree,loc,report_json,fetched_at) VALUES('/remote/repo',1,'{}',1),('/remote/standalone',1,'{}',1)",
                [],
            )
            .unwrap();
        db.conn()
            .execute(
                "INSERT INTO sem_entity(id,file,name,kind,span,source_hash) VALUES('remote-entity','/remote/repo/.worktrees/x/src/lib.rs','item','function','1-1','hash')",
                [],
            )
            .unwrap();

        assert_eq!(
            db.prune_orphan_worktree_caches(ORPHAN_CACHE_SWEEP_MAX_KEYS)
                .unwrap(),
            0
        );
        let sem_count: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM sem_entity WHERE id='remote-entity'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            sem_count, 1,
            "registered descendant semantic row was pruned"
        );
        let loc_count: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM loc_cache", [], |row| row.get(0))
            .unwrap();
        assert_eq!(loc_count, 2, "registered repo roots were pruned");
    }

    #[test]
    fn del_worktree_works_inside_an_existing_transaction() {
        let db = Db::open_memory().unwrap();
        db.put_worktree("tab", "/repo", "/wt/a", "feat", None, None)
            .unwrap();

        db.transaction(|db| db.del_worktree("/wt/a")).unwrap();

        assert!(db.worktree_record("/wt/a").unwrap().is_none());
    }

    /// The LOC cache had no cascade at all, which is how a live DB came to hold
    /// ~200 rows against a dozen real worktrees.
    #[test]
    fn del_worktree_cascades_to_loc_cache() {
        use crate::store::CacheStore;
        let db = Db::open_memory().unwrap();
        db.put_worktree("tab", "/repo", "/wt/a", "feat", None, None)
            .unwrap();
        db.put_worktree("tab", "/repo", "/wt/b", "fix", None, None)
            .unwrap();
        db.put_loc_cache("/wt/a", 100, "{}").unwrap();
        db.put_loc_cache("/wt/b", 200, "{}").unwrap();

        db.del_worktree("/wt/a").unwrap();
        assert!(db.get_loc_cache_entry("/wt/a").unwrap().is_none());
        assert_eq!(db.all_loc_cache_stamps().unwrap().len(), 1);

        db.del_worktrees_for_repo("/repo").unwrap();
        assert!(db.all_loc_cache_stamps().unwrap().is_empty());
    }

    /// A workspace's MAIN checkout is a measured path in its own right (the
    /// background scans cover `workspaces.repo_path`, since a home group never
    /// gets a `worktrees` row), so its caches are keyed by `repo_path` and must
    /// cascade when the workspace goes.
    #[test]
    fn del_workspace_cascades_the_root_measurement_caches() {
        use crate::store::{CacheStore, WorktreeAuxStore};
        let db = Db::open_memory().unwrap();
        db.put_workspace("/repo", "repo", "repo").unwrap();
        db.put_worktree_disk("/repo", 7_000, 1_000).unwrap();
        db.put_loc_cache("/repo", 4_242, "{}").unwrap();
        // An unrelated workspace's rows must survive.
        db.put_worktree_disk("/other", 1, 0).unwrap();

        db.del_workspace("/repo").unwrap();
        assert!(db.get_worktree_disk("/repo").unwrap().is_none());
        assert!(db.get_loc_cache_entry("/repo").unwrap().is_none());
        assert!(db.get_worktree_disk("/other").unwrap().is_some());
    }

    #[test]
    fn loc_cache_stamps_and_delete_round_trip() {
        use crate::store::CacheStore;
        let db = Db::open_memory().unwrap();
        assert!(db.all_loc_cache_stamps().unwrap().is_empty());
        db.put_loc_cache("/wt/a", 1, "{}").unwrap();
        db.put_loc_cache("/wt/b", 2, "{}").unwrap();
        let stamps = db.all_loc_cache_stamps().unwrap();
        assert_eq!(stamps.len(), 2);
        assert!(stamps.contains_key("/wt/a") && stamps.contains_key("/wt/b"));

        db.delete_loc_cache("/wt/a").unwrap();
        assert_eq!(db.all_loc_cache_stamps().unwrap().len(), 1);
        // Deleting an absent key is a no-op, not an error.
        db.delete_loc_cache("/nope").unwrap();
    }

    #[test]
    fn worktree_disk_stamps_bulk_read_matches_the_per_row_read() {
        use crate::store::WorktreeAuxStore;
        let db = Db::open_memory().unwrap();
        db.put_worktree_disk("/wt/a", 10, 2).unwrap();
        let stamps = db.all_worktree_disk_stamps().unwrap();
        let (_, _, at) = db.get_worktree_disk("/wt/a").unwrap().unwrap();
        assert_eq!(stamps.get("/wt/a"), Some(&at));
    }

    /// A terminal's layout row is keyed by whatever session was active when it
    /// was persisted, so re-opening it from ANOTHER project has to find that row
    /// wherever it landed — that lookup is what carries `pane_sessions` into the
    /// restore and turns a fresh shell back into a reattach.
    #[test]
    fn terminal_group_tabs_finds_the_row_from_another_session() {
        use crate::models::{GroupTabRow, TabGroupRow};
        let db = Db::open_memory().unwrap();
        db.put_tab_group(
            "/repo/a",
            &TabGroupRow {
                name: "prod".into(),
                kind: "terminal".into(),
                worktree: String::new(),
                ordinal: 3,
                active_tab: 1,
            },
        )
        .unwrap();
        for (ordinal, sessions) in [
            (
                0i64,
                r#"{"1":{"provider":"daemon","id":"local","session":"s-a"}}"#,
            ),
            (1, ""),
        ] {
            db.put_group_tab(
                "/repo/a",
                &GroupTabRow {
                    group_name: "prod".into(),
                    ordinal,
                    title: "main".into(),
                    pane_tree: "1".into(),
                    focused_pane: 1,
                    pane_cwds: String::new(),
                    pane_cmds: String::new(),
                    pane_sessions: sessions.into(),
                    scrollback_snapshot: String::new(),
                },
            )
            .unwrap();
        }

        let (session, group, tabs) = db
            .terminal_group_tabs("prod")
            .unwrap()
            .expect("the row is found from any session");
        assert_eq!(session, "/repo/a", "the donor is named for the delete");
        assert_eq!(group.active_tab, 1, "the remembered tab comes back too");
        assert_eq!(tabs.len(), 2, "every tab, in order");
        assert!(
            tabs[0].pane_sessions.contains("s-a"),
            "the daemon session survives the lookup"
        );
    }

    /// A same-named WORKTREE group must never answer a terminal lookup: it would
    /// hand a terminal another group's pane tree.
    #[test]
    fn terminal_group_tabs_ignores_a_same_named_worktree_group() {
        use crate::models::{GroupTabRow, TabGroupRow};
        let db = Db::open_memory().unwrap();
        db.put_tab_group(
            "/repo/a",
            &TabGroupRow {
                name: "prod".into(),
                kind: "branch".into(),
                worktree: "/wt/prod".into(),
                ordinal: 0,
                active_tab: 0,
            },
        )
        .unwrap();
        db.put_group_tab(
            "/repo/a",
            &GroupTabRow {
                group_name: "prod".into(),
                ordinal: 0,
                title: "1".into(),
                pane_tree: "1".into(),
                focused_pane: 1,
                pane_cwds: String::new(),
                pane_cmds: String::new(),
                pane_sessions: String::new(),
                scrollback_snapshot: String::new(),
            },
        )
        .unwrap();

        assert!(db.terminal_group_tabs("prod").unwrap().is_none());
        assert!(db.terminal_group_tabs("never-persisted").unwrap().is_none());
    }

    #[test]
    fn delete_tab_groups_for_worktree_keys_on_path() {
        use crate::models::{GroupTabRow, TabGroupRow};
        let db = Db::open_memory().unwrap();
        let sess = "/repo/root";
        let mk = |name: &str, wt: &str| TabGroupRow {
            name: name.into(),
            kind: "branch".into(),
            worktree: wt.into(),
            ordinal: 0,
            active_tab: 0,
        };
        let mktab = |group: &str| GroupTabRow {
            group_name: group.into(),
            ordinal: 0,
            title: "1".into(),
            pane_tree: r#"{"leaf":0}"#.into(),
            focused_pane: 0,
            pane_cwds: String::new(),
            pane_cmds: String::new(),
            pane_sessions: String::new(),
            scrollback_snapshot: String::new(),
        };
        db.put_tab_group(sess, &mk("app/feat", "/wt/feat")).unwrap();
        db.put_tab_group(sess, &mk("app/other", "/wt/other"))
            .unwrap();
        db.put_group_tab(sess, &mktab("app/feat")).unwrap();
        db.put_group_tab(sess, &mktab("app/other")).unwrap();
        // Same worktree path in another session must be untouched.
        db.put_tab_group("elsewhere", &mk("x/feat", "/wt/feat"))
            .unwrap();

        db.delete_tab_groups_for_worktree(sess, "/wt/feat").unwrap();
        let left = db.groups_for_session(sess).unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].name, "app/other");
        // The group's tabs went with it; the other group's tab survived.
        let tabs = db.group_tabs_for_session(sess).unwrap();
        assert_eq!(tabs.len(), 1);
        assert_eq!(tabs[0].group_name, "app/other");
        assert_eq!(db.groups_for_session("elsewhere").unwrap().len(), 1);
        // Unknown path is a no-op, not an error.
        db.delete_tab_groups_for_worktree(sess, "/wt/none").unwrap();
        assert_eq!(db.groups_for_session(sess).unwrap().len(), 1);
    }

    #[test]
    fn worktree_titles_roundtrip_and_upsert() {
        let db = Db::open_memory().unwrap();
        // Empty until something is stored.
        assert!(db.all_worktree_titles().unwrap().is_empty());

        db.set_worktree_window_title("/wt/a", "vim — main.rs")
            .unwrap();
        db.set_worktree_window_title("/wt/b", "cargo test").unwrap();
        let map = db.all_worktree_titles().unwrap();
        assert_eq!(map.get("/wt/a").map(String::as_str), Some("vim — main.rs"));
        assert_eq!(map.get("/wt/b").map(String::as_str), Some("cargo test"));

        // Upsert replaces the title in place (keyed by worktree path).
        db.set_worktree_window_title("/wt/a", "less").unwrap();
        let map = db.all_worktree_titles().unwrap();
        assert_eq!(map.len(), 2);
        assert_eq!(map.get("/wt/a").map(String::as_str), Some("less"));

        // Empty titles are skipped by the bulk load (they'd add no sidebar value).
        db.set_worktree_window_title("/wt/c", "").unwrap();
        assert!(!db.all_worktree_titles().unwrap().contains_key("/wt/c"));
    }

    #[test]
    fn terminal_position_swap_round_trips_and_survives_read() {
        let db = Db::open_memory().unwrap();
        // Insert order is the initial position order (MAX+1 each).
        db.put_terminal("a", "local", "", None).unwrap();
        db.put_terminal("b", "local", "", None).unwrap();
        db.put_terminal("c", "local", "", None).unwrap();
        let names = |db: &Db| -> Vec<String> {
            db.terminals()
                .unwrap()
                .into_iter()
                .map(|t| t.name)
                .collect()
        };
        assert_eq!(names(&db), vec!["a", "b", "c"]);

        // Swap the two adjacent siblings a<->b; the new order persists.
        db.swap_terminal_positions("a", "b").unwrap();
        assert_eq!(names(&db), vec!["b", "a", "c"]);

        // set_terminal_position drives a single row directly.
        db.set_terminal_position("c", -1).unwrap();
        assert_eq!(names(&db), vec!["c", "b", "a"]);
    }

    #[test]
    fn terminal_position_swap_heals_tied_positions() {
        let db = Db::open_memory().unwrap();
        // Simulate legacy rows whose position was never re-assigned (all 0):
        // put_terminal seeds MAX+1, so force a tie directly.
        db.put_terminal("a", "local", "", None).unwrap();
        db.put_terminal("b", "local", "", None).unwrap();
        db.conn()
            .execute("UPDATE terminals SET position=0", [])
            .unwrap();
        // A swap must heal the tie (normalize) then exchange, not no-op.
        db.swap_terminal_positions("a", "b").unwrap();
        let names: Vec<String> = db
            .terminals()
            .unwrap()
            .into_iter()
            .map(|t| t.name)
            .collect();
        assert_eq!(names, vec!["b", "a"]);
    }
}
