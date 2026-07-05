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

    /// Whether superzej already knows this repo (registered, or in recents).
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
    fn swap_workspace_positions(&self, a: &str, b: &str) -> Result<()> {
        // Read both before writing — a self-referential CASE-UPDATE could
        // observe its own intermediate write and clobber the swap.
        let pos = |p: &str| -> Result<Option<i64>> {
            Ok(self
                .conn()
                .query_row(
                    "SELECT position FROM workspaces WHERE repo_path=?1",
                    params![p],
                    |r| r.get::<_, Option<i64>>(0),
                )
                .optional()?
                .flatten())
        };
        if let (Some(pa), Some(pb)) = (pos(a)?, pos(b)?) {
            self.set_workspace_position(a, pb)?;
            self.set_workspace_position(b, pa)?;
        }
        Ok(())
    }

    /// Set one workspace's persisted sort position (repo_path key).
    fn set_workspace_position(&self, repo_path: &str, position: i64) -> Result<()> {
        self.conn().execute(
            "UPDATE workspaces SET position=?2 WHERE repo_path=?1",
            params![repo_path, position],
        )?;
        Ok(())
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
        self.conn().execute(
            r#"INSERT INTO worktrees(worktree,session_name,tab_name,repo_path,branch,agent,created_at,location,position,folder_id)
               VALUES(?1,?2,?3,?4,?5,'',?6,?7,(SELECT COALESCE(MAX(position),-1)+1 FROM worktrees),?8)
               ON CONFLICT(worktree) DO UPDATE SET branch=?5, tab_name=?3, repo_path=?4, session_name=?2, location=?7, folder_id=COALESCE(?8, folder_id)"#,
            params![wt, session(), tab, root, branch, util::now(), location, folder_id],
        )?;
        Ok(())
    }

    /// The remote-location descriptor for a worktree (None/empty = local).
    fn location_for(&self, wt: &str) -> Result<Option<String>> {
        let r = self
            .conn()
            .query_row(
                "SELECT location FROM worktrees WHERE worktree=?1",
                params![wt],
                |r| r.get::<_, Option<String>>(0),
            )
            .ok()
            .flatten();
        Ok(r)
    }

    /// The (local) repo root recorded for a worktree — needed for the per-repo
    /// `.superzej` overlay when the worktree itself lives remote.
    fn repo_root_for(&self, wt: &str) -> Result<Option<String>> {
        let r = self
            .conn()
            .query_row(
                "SELECT repo_path FROM worktrees WHERE worktree=?1",
                params![wt],
                |r| r.get::<_, Option<String>>(0),
            )
            .ok()
            .flatten();
        Ok(r)
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
        Ok(())
    }

    /// Forget every registry worktree row owned by a repo (no disk side
    /// effects). Pairs with [`Self::del_workspace`] so a removed workspace's
    /// cross-workspace rows neither re-render nor resurrect on the next launch.
    fn del_worktrees_for_repo(&self, repo_path: &str) -> Result<()> {
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
        let r = self
            .conn()
            .query_row(
                "SELECT worktree FROM worktrees WHERE session_name=?1 AND tab_name=?2 LIMIT 1",
                params![session, tab],
                |r| r.get::<_, String>(0),
            )
            .ok();
        Ok(r)
    }

    /// All recorded worktrees (metadata only; git supplies live status).
    fn worktrees(&self) -> Result<Vec<WorktreeRow>> {
        // `position` is the persistent sort key (creation order by default,
        // user-reorderable). Order by it so every consumer — the sidebar's
        // unloaded-workspace rows and the resurrect adopt loop — is stable;
        // created_at/path are deterministic tie-breakers for any unset row.
        let mut stmt = self.conn().prepare(
            "SELECT worktree, branch, agent, created_at, repo_path, tab_name, session_name, location, position, sandbox_backend, folder_id, env_name
             FROM worktrees ORDER BY position, created_at, worktree",
        )?;
        let rows = stmt.query_map([], |r| {
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
                folder_id: r.get(10)?,
                env_name: r
                    .get::<_, Option<String>>(11)?
                    .filter(|s| !s.trim().is_empty()),
            })
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// Swap the persisted sort positions of two worktrees (by path). Used by
    /// the sidebar's manual reorder (Shift+Alt+↑/↓): the caller picks the two
    /// adjacent siblings, this exchanges their `position` so the new order
    /// survives restart. Positions are globally unique (migration + MAX+1
    /// inserts), so a swap can never create a collision.
    fn swap_worktree_positions(&self, a: &str, b: &str) -> Result<()> {
        // Read both first, then write — a single CASE-UPDATE that reads the
        // table it mutates can observe its own intermediate write and clobber
        // the swap.
        let pos = |wt: &str| -> Result<Option<i64>> {
            Ok(self
                .conn()
                .query_row(
                    "SELECT position FROM worktrees WHERE worktree=?1",
                    params![wt],
                    |r| r.get::<_, Option<i64>>(0),
                )
                .optional()?
                .flatten())
        };
        if let (Some(pa), Some(pb)) = (pos(a)?, pos(b)?) {
            self.set_worktree_position(a, pb)?;
            self.set_worktree_position(b, pa)?;
        }
        Ok(())
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
    fn ensure_folder(&self, repo_path: &str, name: &str) -> Result<i64> {
        let want = name.trim();
        for f in self.folders_for_workspace(repo_path)? {
            if f.name.trim().eq_ignore_ascii_case(want) {
                return Ok(f.folder_id);
            }
        }
        self.create_folder(repo_path, want)
    }

    /// File (or unfile, with `None`) a single worktree into a folder.
    fn set_worktree_folder(&self, worktree: &str, folder_id: Option<i64>) -> Result<()> {
        self.conn().execute(
            "UPDATE worktrees SET folder_id = ?1 WHERE worktree = ?2",
            params![folder_id, worktree],
        )?;
        Ok(())
    }

    /// Select the named execution environment for a worktree (`[env.<name>]`).
    /// `""` clears it (inherit the workspace/repo/global layer).
    fn set_worktree_env(&self, wt: &str, env: &str) -> Result<()> {
        let val = (!env.trim().is_empty()).then(|| env.trim().to_string());
        self.conn().execute(
            "UPDATE worktrees SET env_name=?2 WHERE worktree=?1",
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
    /// workspace's `env_name`. (`None` ⇒ fall through to repo `.superzej.*` /
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
        let r = self
            .conn()
            .query_row(
                "SELECT spec FROM layouts WHERE name=?1",
                params![name],
                |r| r.get::<_, String>(0),
            )
            .ok();
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
                    COALESCE(sandbox_backend, ''), COALESCE(env_name, '')
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
}
