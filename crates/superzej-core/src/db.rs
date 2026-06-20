//! SQLite-backed state & history (replaces the old JSON files).
//!
//! One global DB at `$XDG_STATE_HOME/superzej/superzej.db`:
//!   repos      — every repo ever opened (the launcher's "recents")
//!   workspaces — a repo opened as a zellij session (one session per repo)
//!   worktrees  — superzej-managed worktrees (one per zellij tab; keyed by path)
//!
//! git is the source of truth for worktrees on disk, and live `zellij
//! list-sessions` for sessions; this is a cache + history layer. rusqlite is
//! bundled, so there's no system sqlite dependency.

use crate::models::{ContainerEvent, WorkspaceRow, WorktreeRow};
use crate::util;
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use std::path::PathBuf;

/// Schema version. v3: workspace / worktree remap. v4 (native host): adds
/// `tab_layout` + `session_state` for DB-driven session resurrect (the native
/// compositor owns layout). v5: adds the `ui_state` key-value table backing the
/// sidebar's persisted view state (collapse, sort mode, bar width, pin order) —
/// purely additive. v6: tabs live *within* a worktree — the flat `tab_layout`
/// (pages encoded as " ·N" name suffixes) becomes `tab_groups` + `group_tabs`;
/// legacy rows are transformed in place and `tab_layout` is dropped.
/// v9: adds `issue_cache` (TTL'd per-repo provider cache) and `issue_links`
/// (worktree↔issue associations for badge/palette surfacing).
/// v10: adds `issue_relations` (blocking/blocked-by/duplicate/relates DAG) and
/// `issue_projects` (sprint/milestone/epic cache per repo+provider).
/// v11: adds `notifications` inbox (kind, issue ref, message, read flag).
/// v12: adds `agent_dispatches` (AI agent assignments: issue→worktree→agent).
const SCHEMA_VERSION: i64 = 12;

pub struct Db {
    conn: Connection,
}

fn db_path() -> PathBuf {
    util::xdg_state_home().join("superzej/superzej.db")
}

/// The current session marker (the repo path the host runs against, or "default"
/// when unset). Recorded on worktree rows; the native host keys workspaces by
/// repo path, so this is a coarse fallback only.
pub fn session() -> String {
    std::env::var("SUPERZEJ_SESSION").unwrap_or_else(|_| "default".into())
}

impl Db {
    pub fn open() -> Result<Db> {
        let path = db_path();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        Self::init(Connection::open(&path)?)
    }

    /// An isolated in-memory DB (tests): same schema/migration, no file.
    pub fn open_memory() -> Result<Db> {
        Self::init(Connection::open_in_memory()?)
    }

    /// Open at an explicit path: exercises the real file-backed `open()` path
    /// (dir creation + on-disk connection + migration) without mutating the
    /// process-global `XDG_STATE_HOME`. Used by tests and by host integration
    /// tests across the workspace, hence `pub`.
    pub fn open_at(path: &std::path::Path) -> Result<Db> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        Self::init(Connection::open(path)?)
    }

    /// Apply pragmas, migration, and schema to a fresh connection.
    fn init(conn: Connection) -> Result<Db> {
        conn.busy_timeout(std::time::Duration::from_millis(5000))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        // WAL + synchronous=NORMAL: commits stop fsyncing the WAL (only
        // checkpoints sync). Cold-start schema creation alone was ~25 serial
        // fsyncs (~130ms of the launch budget) under the FULL default. The DB
        // is a cache/resurrection layer — git is the source of truth — so
        // NORMAL's failure mode (an OS crash may drop the last commits, never
        // corrupt) is the right trade.
        conn.pragma_update(None, "synchronous", "NORMAL")?;

        // Migrate. v2→v3 collapses the per-repo-session model into one session
        // where each repo/worktree is a tab, so `workspaces` is re-keyed by
        // repo_path (was session_name) and `worktrees.session_name` becomes the
        // single UI session. Neither has a faithful transform — drop and
        // recreate. The `repos` recents history is preserved (it's the only
        // irreplaceable data); git + live tabs re-discover everything else.
        let ver: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap_or(0);
        // The v2→v3 remap has no faithful transform — drop & recreate. Guard it
        // to `ver < 3` so later, purely-additive bumps (v3→v4: new `tab_layout`
        // /`session_state` tables, created below) don't wipe a v3 user's data.
        if ver < 3 {
            conn.execute_batch(
                "DROP TABLE IF EXISTS tabs;
                 DROP TABLE IF EXISTS worktrees;
                 DROP TABLE IF EXISTS workspaces;",
            )?;
            // Add the session_name column to a pre-existing repos table (no-op /
            // ignored error on a fresh DB, where the CREATE below adds it).
            let _ = conn.execute("ALTER TABLE repos ADD COLUMN session_name TEXT", []);
        }
        if ver < SCHEMA_VERSION {
            conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        }

        let _ = conn.execute("ALTER TABLE worktrees ADD COLUMN sandbox_backend TEXT", []);

        // One transaction for the whole schema: execute_batch otherwise
        // autocommits per statement — a dozen WAL commits where one will do.
        conn.execute_batch(
            r#"
            BEGIN;
            CREATE TABLE IF NOT EXISTS repos (
              path         TEXT PRIMARY KEY,
              name         TEXT,
              first_seen   INTEGER,
              last_opened  INTEGER,
              open_count   INTEGER DEFAULT 0,
              seq          INTEGER DEFAULT 0,
              session_name TEXT
            );
            CREATE TABLE IF NOT EXISTS workspaces (
              repo_path    TEXT PRIMARY KEY,
              name         TEXT,
              created_at   INTEGER,
              last_active  INTEGER
            );
            CREATE TABLE IF NOT EXISTS worktrees (
              worktree     TEXT PRIMARY KEY,
              session_name TEXT,
              tab_name     TEXT,
              repo_path    TEXT,
              branch       TEXT,
              agent        TEXT,
              created_at   INTEGER,
              location     TEXT,
              sandbox_backend TEXT
            );
            CREATE TABLE IF NOT EXISTS pr_cache (
              worktree   TEXT PRIMARY KEY,
              branch     TEXT,
              json       TEXT,
              fetched_at INTEGER
            );
            -- Last computed `diff --files` TSV per worktree, so the panel can
            -- paint instantly from cache (via `panel-snapshot`) and hydrate live.
            CREATE TABLE IF NOT EXISTS diff_cache (
              worktree   TEXT PRIMARY KEY,
              files      TEXT,
              fetched_at INTEGER
            );
            -- Latest structured commit feed per worktree. The host paints the
            -- commits panel from this cache immediately, then refreshes it on a
            -- background worker so `git log` never gates opening the sidebar.
            CREATE TABLE IF NOT EXISTS commit_cache (
              worktree   TEXT PRIMARY KEY,
              json       TEXT,
              fetched_at INTEGER
            );
            CREATE TABLE IF NOT EXISTS loc_cache (
              worktree   TEXT PRIMARY KEY,
              loc        INTEGER,
              fetched_at INTEGER
            );
            -- Latest test-explorer state per worktree. This is a cache, not a
            -- history log: full timelines live in the later activity/audit layer.
            CREATE TABLE IF NOT EXISTS test_cache (
              worktree   TEXT PRIMARY KEY,
              json       TEXT,
              fetched_at INTEGER
            );
            -- A stable, globally-unique slug per repo: the prefix of every tab
            -- that repo owns (`{slug}/…`). Assigned once with collision suffixing
            -- so two repos with the same basename get distinct tabs.
            CREATE TABLE IF NOT EXISTS repo_slugs (
              repo_path TEXT PRIMARY KEY,
              slug      TEXT NOT NULL
            );
            -- Command-palette frecency: how often / how recently each action or
            -- nav target was chosen, so the palette floats them up on an empty
            -- query. `key` is the row's stable frecency key (e.g. "new-worktree",
            -- "wt:/path", "repo:/path").
            CREATE TABLE IF NOT EXISTS palette_usage (
              key        TEXT PRIMARY KEY,
              count      INTEGER DEFAULT 0,
              last_used  INTEGER
            );
            -- v6: the native host owns the layout. A worktree group is one
            -- sidebar worktree owning an ordered set of tabs; each tab carries
            -- its serialized pane tree (CenterTree JSON) and focused leaf —
            -- enough to rebuild every worktree and tab on resurrect.
            CREATE TABLE IF NOT EXISTS tab_groups (
              session_name TEXT NOT NULL,
              name         TEXT NOT NULL,
              kind         TEXT NOT NULL,
              worktree     TEXT NOT NULL,
              ordinal      INTEGER NOT NULL,
              active_tab   INTEGER NOT NULL DEFAULT 0,
              PRIMARY KEY (session_name, name)
            );
            CREATE TABLE IF NOT EXISTS group_tabs (
              session_name TEXT NOT NULL,
              group_name   TEXT NOT NULL,
              ordinal      INTEGER NOT NULL,
              title        TEXT NOT NULL,
              pane_tree    TEXT NOT NULL,
              focused_pane INTEGER NOT NULL DEFAULT 0,
              PRIMARY KEY (session_name, group_name, ordinal)
            );
            -- v4: which tab (v6: which worktree group) was active at exit.
            CREATE TABLE IF NOT EXISTS session_state (
              session_name TEXT PRIMARY KEY,
              active_tab   TEXT,
              updated_at   INTEGER
            );
            -- v5: a small key-value store for the sidebar's persisted view
            -- state. `scope` namespaces a key (session_name, a workspace slug,
            -- or "" for global); `key` is e.g. "collapse:<slug>", "sort_mode",
            -- "sidebar_cols", "pin:<slug>", "pin_ordinal:<slug>". Survives
            -- session resurrection alongside the rest of the layout.
            CREATE TABLE IF NOT EXISTS ui_state (
              scope TEXT NOT NULL,
              key   TEXT NOT NULL,
              value TEXT,
              PRIMARY KEY (scope, key)
            );
            -- Switch/panel-resolve hot path: worktree lookup keyed by the tab.
            CREATE INDEX IF NOT EXISTS idx_worktrees_session_tab
              ON worktrees (session_name, tab_name);
            -- v7: reflog undo bookkeeping — the reset targets WE wrote, so the
            -- undo planner can tell its own resets from user actions (capped
            -- per worktree on insert).
            CREATE TABLE IF NOT EXISTS undo_marks (
              worktree TEXT NOT NULL,
              sha      TEXT NOT NULL,
              ts       INTEGER NOT NULL,
              PRIMARY KEY (worktree, sha)
            );
            -- v7: open-PRs-by-branch cache per repo (JSON array), so branch
            -- rows can render PR badges without a network call.
            CREATE TABLE IF NOT EXISTS pr_branch_cache (
              repo_root  TEXT PRIMARY KEY,
              json       TEXT,
              fetched_at INTEGER
            );
            -- v9: cached issue list per (repo, provider). The JSON column holds
            -- a `Vec<Issue>` array; the host panel reads from this cache
            -- immediately on open (zero network latency) and a background worker
            -- refreshes it on a 60s interval.
            CREATE TABLE IF NOT EXISTS issue_cache (
              repo_root  TEXT    NOT NULL,
              provider   TEXT    NOT NULL,
              json       TEXT    NOT NULL,
              fetched_at INTEGER NOT NULL,
              PRIMARY KEY (repo_root, provider)
            );
            -- v9: which issues the user has explicitly linked to a worktree,
            -- surfaced as tabbar badges and palette quick-links.
            CREATE TABLE IF NOT EXISTS issue_links (
              worktree_path TEXT    NOT NULL,
              issue_id      TEXT    NOT NULL,
              linked_at     INTEGER NOT NULL,
              PRIMARY KEY (worktree_path, issue_id)
            );
            -- v10: directional blocking relationships between issues.
            CREATE TABLE IF NOT EXISTS issue_relations (
              issue_id   TEXT    NOT NULL,
              related_id TEXT    NOT NULL,
              kind       TEXT    NOT NULL,
              provider   TEXT    NOT NULL,
              fetched_at INTEGER NOT NULL,
              PRIMARY KEY (issue_id, related_id, kind)
            );
            -- v10: project/sprint/milestone cache per repo+provider.
            CREATE TABLE IF NOT EXISTS issue_projects (
              repo_root  TEXT    NOT NULL,
              provider   TEXT    NOT NULL,
              json       TEXT    NOT NULL,
              fetched_at INTEGER NOT NULL,
              PRIMARY KEY (repo_root, provider)
            );
            -- v11: notification inbox. Rows accumulate from the diff engine;
            -- the panel inbox marks them read.
            CREATE TABLE IF NOT EXISTS notifications (
              id             INTEGER PRIMARY KEY AUTOINCREMENT,
              kind           TEXT    NOT NULL,
              issue_id       TEXT    NOT NULL,
              message        TEXT    NOT NULL,
              created_at_ms  INTEGER NOT NULL,
              read           INTEGER NOT NULL DEFAULT 0,
              worktree_path  TEXT    NOT NULL DEFAULT ''
            );
            -- v12: agent dispatch registry.  Each row tracks one AI coding
            -- agent assigned to work on one issue in a dedicated worktree.
            CREATE TABLE IF NOT EXISTS agent_dispatches (
              id               INTEGER PRIMARY KEY AUTOINCREMENT,
              issue_id         TEXT    NOT NULL,
              worktree_path    TEXT    NOT NULL,
              agent_name       TEXT    NOT NULL,
              dispatched_at_ms INTEGER NOT NULL,
              status           TEXT    NOT NULL DEFAULT 'queued'
            );
            -- v13: sandbox audit trail.  Exec events (commands run inside
            -- containers), network events (outbound connections), and GC events
            -- (orphan teardown) from the sandbox subsystem.
            CREATE TABLE IF NOT EXISTS container_events (
              id        INTEGER PRIMARY KEY AUTOINCREMENT,
              worktree  TEXT    NOT NULL,
              ts        INTEGER NOT NULL,
              kind      TEXT    NOT NULL,
              detail    TEXT,
              exit_code INTEGER
            );
            CREATE INDEX IF NOT EXISTS idx_container_events_wt
              ON container_events (worktree, ts DESC);
            COMMIT;
            "#,
        )?;
        // Additive: a pre-existing v3 worktrees table predates the remote-worktree
        // `location` column. Add it in place (ignored if already present) so local
        // worktree history survives — no full migration/reset needed.
        let _ = conn.execute("ALTER TABLE worktrees ADD COLUMN location TEXT", []);
        // Additive: running-pin set per session (JSON), so the native host can
        // resurrect strip/float pins (the pin supervisor re-launches them).
        let _ = conn.execute("ALTER TABLE session_state ADD COLUMN pin_state TEXT", []);
        // Additive: a workspace's kind — "repo" (a git repo) or "dir" (a plain
        // non-git directory). Defaults keep every pre-existing workspace a repo.
        let _ = conn.execute(
            "ALTER TABLE workspaces ADD COLUMN kind TEXT DEFAULT 'repo'",
            [],
        );
        // v8: a persistent per-worktree sort key — the single source of truth
        // for sidebar order (loaded + unloaded). Additive; backfilled below.
        let _ = conn.execute("ALTER TABLE worktrees ADD COLUMN position INTEGER", []);
        // Backfill any unset positions deterministically by creation order
        // (path as the tie-breaker), giving pre-v8 worktrees a stable,
        // collision-free order on first launch after upgrade. Runs once: after
        // this every row has a position, and `put_worktree` assigns MAX+1.
        let _ = conn.execute(
            "UPDATE worktrees SET position = (
                 SELECT COUNT(*) FROM worktrees AS w2
                 WHERE (w2.created_at, w2.worktree) < (worktrees.created_at, worktrees.worktree)
             ) WHERE position IS NULL",
            [],
        );
        // v6: transform any remaining flat v4/v5 `tab_layout` into worktree
        // groups. Keyed on the legacy table's existence (not the version) so
        // it is idempotent and a failed earlier attempt retries next open.
        migrate_tab_layout_v6(&conn);
        Ok(Db { conn })
    }

    // --- PR status cache (TTL'd; feeds the right panel) --------------------
    pub fn get_pr_cache(&self, worktree: &str) -> Result<Option<(String, i64)>> {
        let r = self
            .conn
            .query_row(
                "SELECT json, fetched_at FROM pr_cache WHERE worktree=?1",
                params![worktree],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
            )
            .ok();
        Ok(r)
    }

    pub fn put_pr_cache(&self, worktree: &str, branch: &str, json: &str) -> Result<()> {
        self.conn.execute(
            r#"INSERT INTO pr_cache(worktree,branch,json,fetched_at)
               VALUES(?1,?2,?3,?4)
               ON CONFLICT(worktree) DO UPDATE SET branch=?2, json=?3, fetched_at=?4"#,
            params![worktree, branch, json, util::now()],
        )?;
        Ok(())
    }

    // --- per-repo open-PRs-by-branch cache (feeds branch-row PR badges) ----
    pub fn get_pr_branch_cache(&self, repo_root: &str) -> Result<Option<(String, i64)>> {
        let r = self
            .conn
            .query_row(
                "SELECT json, fetched_at FROM pr_branch_cache WHERE repo_root=?1",
                params![repo_root],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
            )
            .ok();
        Ok(r)
    }

    pub fn put_pr_branch_cache(&self, repo_root: &str, json: &str) -> Result<()> {
        self.conn.execute(
            r#"INSERT INTO pr_branch_cache(repo_root,json,fetched_at)
               VALUES(?1,?2,?3)
               ON CONFLICT(repo_root) DO UPDATE SET json=?2, fetched_at=?3"#,
            params![repo_root, json, util::now()],
        )?;
        Ok(())
    }

    /// Open PR counts grouped by branch (`head_ref`) for a repo, parsed from the
    /// per-repo `pr_branch_cache`. Used to surface PR badges on sidebar rows
    /// (item 28): the host maps each worktree's branch to its count. Only PRs in
    /// the `OPEN` state are counted. Returns an empty map when the cache is
    /// absent or unparseable.
    pub fn get_open_pr_counts_by_branch(
        &self,
        repo_root: &str,
    ) -> Result<std::collections::BTreeMap<String, usize>> {
        let mut counts = std::collections::BTreeMap::new();
        let Some((json, _)) = self.get_pr_branch_cache(repo_root)? else {
            return Ok(counts);
        };
        for pr in crate::github::parse_pr_headers(&json) {
            if pr.state.eq_ignore_ascii_case("open") {
                *counts.entry(pr.head_ref).or_insert(0) += 1;
            }
        }
        Ok(counts)
    }

    // --- issue tracker cache (TTL'd, per repo+provider) ---------------------
    pub fn get_issue_cache(
        &self,
        repo_root: &str,
        provider: &str,
    ) -> Result<Option<(String, i64)>> {
        let r = self
            .conn
            .query_row(
                "SELECT json, fetched_at FROM issue_cache WHERE repo_root=?1 AND provider=?2",
                params![repo_root, provider],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
            )
            .ok();
        Ok(r)
    }

    pub fn put_issue_cache(&self, repo_root: &str, provider: &str, json: &str) -> Result<()> {
        self.conn.execute(
            r#"INSERT INTO issue_cache(repo_root,provider,json,fetched_at)
               VALUES(?1,?2,?3,?4)
               ON CONFLICT(repo_root,provider) DO UPDATE SET json=?3, fetched_at=?4"#,
            params![repo_root, provider, json, util::now()],
        )?;
        Ok(())
    }

    // --- worktree↔issue links (badge + palette surfacing) -------------------
    /// Associate `issue_id` (in `"<provider>:<key>"` form) with a worktree path.
    pub fn link_issue(&self, worktree_path: &str, issue_id: &str) -> Result<()> {
        self.conn.execute(
            r#"INSERT INTO issue_links(worktree_path,issue_id,linked_at)
               VALUES(?1,?2,?3)
               ON CONFLICT(worktree_path,issue_id) DO UPDATE SET linked_at=?3"#,
            params![worktree_path, issue_id, util::now()],
        )?;
        Ok(())
    }

    /// Remove a worktree↔issue association.
    pub fn unlink_issue(&self, worktree_path: &str, issue_id: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM issue_links WHERE worktree_path=?1 AND issue_id=?2",
            params![worktree_path, issue_id],
        )?;
        Ok(())
    }

    /// All issue ids linked to a worktree, newest first.
    pub fn linked_issues(&self, worktree_path: &str) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT issue_id FROM issue_links WHERE worktree_path=?1 ORDER BY linked_at DESC",
        )?;
        let rows = stmt.query_map(params![worktree_path], |r| r.get::<_, String>(0))?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    // --- notifications inbox -------------------------------------------------

    /// Append a notification.  Returns the new row id.
    pub fn put_notification(
        &self,
        kind: &str,
        issue_id: &str,
        message: &str,
        worktree_path: &str,
    ) -> Result<i64> {
        self.conn.execute(
            r#"INSERT INTO notifications(kind,issue_id,message,created_at_ms,read,worktree_path)
               VALUES(?1,?2,?3,?4,0,?5)"#,
            params![kind, issue_id, message, util::now(), worktree_path],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// All unread notifications, newest first.
    pub fn get_unread_notifications(&self) -> Result<Vec<crate::notification::Notification>> {
        self.notifications_query(
            "SELECT id,kind,issue_id,message,created_at_ms,read,worktree_path \
             FROM notifications WHERE read=0 ORDER BY created_at_ms DESC",
            rusqlite::params![],
            usize::MAX,
        )
    }

    /// All notifications (read and unread), newest first, capped at `limit`.
    pub fn get_all_notifications(
        &self,
        limit: usize,
    ) -> Result<Vec<crate::notification::Notification>> {
        self.notifications_query(
            "SELECT id,kind,issue_id,message,created_at_ms,read,worktree_path \
             FROM notifications ORDER BY created_at_ms DESC",
            rusqlite::params![],
            limit,
        )
    }

    fn notifications_query(
        &self,
        sql: &str,
        _params: &[&dyn rusqlite::ToSql],
        limit: usize,
    ) -> Result<Vec<crate::notification::Notification>> {
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, i64>(5)?,
                r.get::<_, String>(6)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows.filter_map(|r| r.ok()) {
            if out.len() >= limit {
                break;
            }
            let kind: crate::notification::NotificationKind =
                serde_json::from_str(&format!("\"{}\"", row.1))
                    .unwrap_or(crate::notification::NotificationKind::StatusChanged);
            out.push(crate::notification::Notification {
                id: row.0,
                kind,
                source_ref: row.2,
                message: row.3,
                created_at_ms: row.4,
                read: row.5 != 0,
                worktree_path: row.6,
            });
        }
        Ok(out)
    }

    /// Mark a single notification as read.
    pub fn mark_notification_read(&self, id: i64) -> Result<()> {
        self.conn
            .execute("UPDATE notifications SET read=1 WHERE id=?1", params![id])?;
        Ok(())
    }

    /// Mark all notifications as read.
    pub fn mark_all_notifications_read(&self) -> Result<()> {
        self.conn.execute("UPDATE notifications SET read=1", [])?;
        Ok(())
    }

    /// Get unread notification counts grouped by worktree_path.
    /// Returns a map from worktree_path to count of unread notifications.
    pub fn get_unread_counts_by_worktree(
        &self,
    ) -> Result<std::collections::BTreeMap<String, usize>> {
        let mut stmt = self.conn.prepare(
            "SELECT worktree_path, COUNT(*) FROM notifications \
             WHERE read=0 AND worktree_path != '' \
             GROUP BY worktree_path",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
        let mut counts = std::collections::BTreeMap::new();
        for row in rows.filter_map(|r| r.ok()) {
            counts.insert(row.0, row.1 as usize);
        }
        Ok(counts)
    }

    /// Get alert counts (test_failed, agent_failed, log_error, process_failed
    /// notifications) grouped by worktree_path. Returns a map from
    /// worktree_path to alert count.
    pub fn get_alert_counts_by_worktree(
        &self,
    ) -> Result<std::collections::BTreeMap<String, usize>> {
        let mut stmt = self.conn.prepare(
            "SELECT worktree_path, COUNT(*) FROM notifications \
             WHERE read=0 AND kind IN ('test_failed', 'agent_failed', 'log_error', 'process_failed') \
             GROUP BY worktree_path",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
        let mut counts = std::collections::BTreeMap::new();
        for row in rows.filter_map(|r| r.ok()) {
            counts.insert(row.0, row.1 as usize);
        }
        Ok(counts)
    }

    // --- agent dispatch registry ---------------------------------------------

    /// Record a new agent dispatch.  Returns the new row id.
    pub fn put_agent_dispatch(
        &self,
        issue_id: &str,
        worktree_path: &str,
        agent_name: &str,
    ) -> Result<i64> {
        self.conn.execute(
            r#"INSERT INTO agent_dispatches(issue_id,worktree_path,agent_name,dispatched_at_ms,status)
               VALUES(?1,?2,?3,?4,'queued')"#,
            params![issue_id, worktree_path, agent_name, util::now()],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Update the status of a dispatch.
    pub fn update_dispatch_status(&self, id: i64, status: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE agent_dispatches SET status=?1 WHERE id=?2",
            params![status, id],
        )?;
        Ok(())
    }

    /// Find the dispatch id for a worktree path (most recent, if any).
    pub fn dispatch_for_worktree(&self, worktree_path: &str) -> Result<Option<i64>> {
        Ok(self.conn
            .query_row(
                "SELECT id FROM agent_dispatches WHERE worktree_path=?1 ORDER BY dispatched_at_ms DESC, id DESC LIMIT 1",
                params![worktree_path],
                |r| r.get::<_, i64>(0),
            )
            .optional()?)
    }

    /// Find the dispatch id and originating issue id for a worktree path.
    pub fn dispatch_info_for_worktree(&self, worktree_path: &str) -> Result<Option<(i64, String)>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id, issue_id FROM agent_dispatches WHERE worktree_path=?1 \
                 ORDER BY dispatched_at_ms DESC, id DESC LIMIT 1",
                params![worktree_path],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
            )
            .optional()?)
    }

    /// Delete a single notification row (dismiss).
    pub fn delete_notification(&self, id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM notifications WHERE id=?1", params![id])?;
        Ok(())
    }

    // --- reflog-undo bookkeeping (which resets are OURS, per worktree) ------
    /// Record a reset target we are about to create, pruning each worktree's
    /// mark set to the freshest 100 (the undo planner only reads ~100 reflog
    /// entries anyway).
    pub fn add_undo_mark(&self, worktree: &str, sha: &str) -> Result<()> {
        self.conn.execute(
            r#"INSERT INTO undo_marks(worktree,sha,ts) VALUES(?1,?2,?3)
               ON CONFLICT(worktree,sha) DO UPDATE SET ts=?3"#,
            params![worktree, sha, util::now()],
        )?;
        self.conn.execute(
            r#"DELETE FROM undo_marks WHERE worktree=?1 AND sha NOT IN (
                 SELECT sha FROM undo_marks WHERE worktree=?1
                 ORDER BY ts DESC LIMIT 100)"#,
            params![worktree],
        )?;
        Ok(())
    }

    /// All recorded undo-reset targets for a worktree (newest first).
    pub fn undo_marks(&self, worktree: &str) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT sha FROM undo_marks WHERE worktree=?1 ORDER BY ts DESC")?;
        let rows = stmt.query_map(params![worktree], |r| r.get::<_, String>(0))?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    // --- diff cache (per worktree; feeds panel-snapshot's instant paint) ----
    pub fn get_diff_cache(&self, worktree: &str) -> Result<Option<(String, i64)>> {
        let r = self
            .conn
            .query_row(
                "SELECT files, fetched_at FROM diff_cache WHERE worktree=?1",
                params![worktree],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
            )
            .ok();
        Ok(r)
    }

    pub fn put_diff_cache(&self, worktree: &str, files: &str) -> Result<()> {
        self.conn.execute(
            r#"INSERT INTO diff_cache(worktree,files,fetched_at)
               VALUES(?1,?2,?3)
               ON CONFLICT(worktree) DO UPDATE SET files=?2, fetched_at=?3"#,
            params![worktree, files, util::now()],
        )?;
        Ok(())
    }

    // --- commit cache (per worktree; feeds instant lazy commits panel) -----
    pub fn get_commit_cache(&self, worktree: &str) -> Result<Option<(String, i64)>> {
        let r = self
            .conn
            .query_row(
                "SELECT json, fetched_at FROM commit_cache WHERE worktree=?1",
                params![worktree],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
            )
            .ok();
        Ok(r)
    }

    pub fn put_commit_cache(&self, worktree: &str, json: &str) -> Result<()> {
        self.conn.execute(
            r#"INSERT INTO commit_cache(worktree,json,fetched_at)
               VALUES(?1,?2,?3)
               ON CONFLICT(worktree) DO UPDATE SET json=?2, fetched_at=?3"#,
            params![worktree, json, util::now()],
        )?;
        Ok(())
    }

    // --- latest test cache (per worktree; feeds Tests panel + sidebar rollups) -
    pub fn get_test_cache(&self, worktree: &str) -> Result<Option<(String, i64)>> {
        let r = self
            .conn
            .query_row(
                "SELECT json, fetched_at FROM test_cache WHERE worktree=?1",
                params![worktree],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
            )
            .ok();
        Ok(r)
    }

    pub fn put_test_cache(&self, worktree: &str, json: &str) -> Result<()> {
        self.conn.execute(
            r#"INSERT INTO test_cache(worktree,json,fetched_at)
               VALUES(?1,?2,?3)
               ON CONFLICT(worktree) DO UPDATE SET json=?2, fetched_at=?3"#,
            params![worktree, json, util::now()],
        )?;
        Ok(())
    }

    // --- LOC cache ---------------------------------------------------------
    pub fn get_loc_cache(&self, worktree: &str) -> Result<Option<usize>> {
        let r = self
            .conn
            .query_row(
                "SELECT loc FROM loc_cache WHERE worktree=?1",
                params![worktree],
                // rusqlite 0.40 dropped the usize SQL impls; store/read as i64.
                |r| r.get::<_, i64>(0).map(|n| n as usize),
            )
            .ok();
        Ok(r)
    }

    /// As [`Db::get_loc_cache`], with the fetch timestamp (for TTL refresh).
    pub fn get_loc_cache_entry(&self, worktree: &str) -> Result<Option<(usize, i64)>> {
        let r = self
            .conn
            .query_row(
                "SELECT loc, fetched_at FROM loc_cache WHERE worktree=?1",
                params![worktree],
                |r| Ok((r.get::<_, i64>(0)? as usize, r.get::<_, i64>(1)?)),
            )
            .ok();
        Ok(r)
    }

    pub fn put_loc_cache(&self, worktree: &str, loc: usize) -> Result<()> {
        self.conn.execute(
            r#"INSERT INTO loc_cache(worktree,loc,fetched_at)
               VALUES(?1,?2,?3)
               ON CONFLICT(worktree) DO UPDATE SET loc=?2, fetched_at=?3"#,
            params![worktree, loc as i64, util::now()],
        )?;
        Ok(())
    }

    /// The recorded agent for a worktree (for `pick-agent --resume` on restart).
    pub fn worktree_agent(&self, worktree: &str) -> Result<Option<String>> {
        let r = self
            .conn
            .query_row(
                "SELECT agent FROM worktrees WHERE worktree=?1",
                params![worktree],
                |r| r.get::<_, String>(0),
            )
            .ok()
            .filter(|s: &String| !s.is_empty());
        Ok(r)
    }

    /// Run `f` inside a single SQLite transaction: commit on `Ok`, roll back
    /// on `Err` (the dropped transaction rolls back). Multi-statement writes
    /// (e.g. persisting a whole session's tab list) must use this so a crash
    /// mid-sequence can't leave a torn half-write — and batched writes pay one
    /// fsync instead of one per statement. Uses `unchecked_transaction`
    /// because `Db` methods take `&self`; do NOT nest `transaction` calls
    /// (SQLite has no nested BEGIN).
    pub fn transaction<T>(&self, f: impl FnOnce(&Db) -> Result<T>) -> Result<T> {
        let tx = self.conn.unchecked_transaction()?;
        let out = f(self)?;
        tx.commit()?;
        Ok(out)
    }

    // --- repo history (launcher recents) -----------------------------------
    pub fn touch_repo(&self, path: &str, name: &str) -> Result<()> {
        let now = util::now();
        // `seq` is a monotonic logical clock so recents ordering stays correct
        // even when several repos are opened in the same wall-clock second.
        self.conn.execute(
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

    pub fn recent_repos(&self, limit: i64) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT path FROM repos ORDER BY seq DESC LIMIT ?1")?;
        let rows = stmt.query_map([limit], |r| r.get::<_, String>(0))?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    pub fn known_repos(&self) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
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

    // --- workspaces (a registered repo or plain dir) ----------------------
    /// Record (or refresh) a registered workspace. Keyed by path — all
    /// workspaces share the one UI session. `kind` is `"repo"` (a git repo) or
    /// `"dir"` (a plain non-git directory); it is set only on first insert, so a
    /// later refresh never downgrades a known workspace's kind.
    pub fn put_workspace(&self, repo_path: &str, name: &str, kind: &str) -> Result<()> {
        let now = util::now();
        self.conn.execute(
            r#"INSERT INTO workspaces(repo_path,name,created_at,last_active,kind)
               VALUES(?1,?2,?3,?3,?4)
               ON CONFLICT(repo_path) DO UPDATE SET name=?2, last_active=?3"#,
            params![repo_path, name, now, kind],
        )?;
        Ok(())
    }

    /// A stable, globally-unique slug for a repo (the prefix of all its tabs).
    /// Reuses the previously-assigned slug; otherwise takes `base`, suffixing
    /// `-2`, `-3`, … on collision with a *different* repo, then persists it.
    /// Two repos with the same basename therefore get distinct tab namespaces.
    pub fn slug_for_repo(&self, repo_path: &str, base: &str) -> Result<String> {
        // One transaction around the read-check-insert so two processes can't
        // both pass the uniqueness scan and claim the same slug.
        self.transaction(|db| {
            if let Ok(s) = db.conn.query_row(
                "SELECT slug FROM repo_slugs WHERE repo_path=?1",
                params![repo_path],
                |r| r.get::<_, String>(0),
            ) && !s.is_empty()
            {
                return Ok(s);
            }
            let taken: std::collections::HashSet<String> = {
                let mut stmt = db
                    .conn
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
            db.conn.execute(
                "INSERT OR REPLACE INTO repo_slugs(repo_path, slug) VALUES(?1, ?2)",
                params![repo_path, cand],
            )?;
            Ok(cand)
        })
    }

    /// Whether superzej already knows this repo (registered, or in recents).
    pub fn is_known_repo(&self, repo_path: &str) -> Result<bool> {
        let found: i64 = self
            .conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM workspaces WHERE repo_path=?1)
                     OR EXISTS(SELECT 1 FROM repos WHERE path=?1)",
                params![repo_path],
                |r| r.get(0),
            )
            .unwrap_or(0);
        Ok(found != 0)
    }

    /// All registered repos (for the sidebar / `list`), newest-active first.
    pub fn workspaces(&self) -> Result<Vec<WorkspaceRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT repo_path, name, created_at, last_active, kind
             FROM workspaces ORDER BY last_active DESC",
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

    // --- command-palette frecency -----------------------------------------
    /// Record that `key` was just chosen (increment count, stamp last_used).
    pub fn bump_palette_usage(&self, key: &str) -> Result<()> {
        self.conn.execute(
            r#"INSERT INTO palette_usage(key,count,last_used)
               VALUES(?1,1,?2)
               ON CONFLICT(key) DO UPDATE SET count=count+1, last_used=?2"#,
            params![key, util::now()],
        )?;
        Ok(())
    }

    /// All usage rows as (key, count, last_used), for frecency ranking.
    pub fn palette_usage(&self) -> Result<Vec<(String, i64, i64)>> {
        let mut stmt = self
            .conn
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

    // --- worktrees (one per tab; keyed by worktree path) -------------------
    /// Record a worktree. `location` is the remote descriptor (JSON) for a remote
    /// worktree, or `None`/empty for an ordinary on-host one.
    pub fn put_worktree(
        &self,
        tab: &str,
        root: &str,
        wt: &str,
        branch: &str,
        location: Option<&str>,
    ) -> Result<()> {
        // New worktrees append at the bottom (MAX+1); an upsert leaves the
        // existing `position` untouched so a re-register never reshuffles order.
        self.conn.execute(
            r#"INSERT INTO worktrees(worktree,session_name,tab_name,repo_path,branch,agent,created_at,location,position)
               VALUES(?1,?2,?3,?4,?5,'',?6,?7,(SELECT COALESCE(MAX(position),-1)+1 FROM worktrees))
               ON CONFLICT(worktree) DO UPDATE SET branch=?5, tab_name=?3, repo_path=?4, session_name=?2, location=?7"#,
            params![wt, session(), tab, root, branch, util::now(), location],
        )?;
        Ok(())
    }

    /// The remote-location descriptor for a worktree (None/empty = local).
    pub fn location_for(&self, wt: &str) -> Result<Option<String>> {
        let r = self
            .conn
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
    pub fn repo_root_for(&self, wt: &str) -> Result<Option<String>> {
        let r = self
            .conn
            .query_row(
                "SELECT repo_path FROM worktrees WHERE worktree=?1",
                params![wt],
                |r| r.get::<_, Option<String>>(0),
            )
            .ok()
            .flatten();
        Ok(r)
    }

    pub fn set_worktree_agent(&self, wt: &str, agent: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE worktrees SET agent=?2 WHERE worktree=?1",
            params![wt, agent],
        )?;
        Ok(())
    }

    pub fn del_worktree(&self, wt: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM worktrees WHERE worktree=?1", params![wt])?;
        Ok(())
    }

    /// Re-key a worktree registry row after a rename (`git branch -m` +
    /// `git worktree move`): the primary key `worktree` (path) moves to
    /// `new_path`, and the `tab_name`/`branch` follow the new branch. `position`,
    /// `agent`, and `sandbox_backend` are preserved. No-op if the old row is gone.
    pub fn rename_worktree(
        &self,
        old_path: &str,
        new_path: &str,
        new_tab: &str,
        new_branch: &str,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE worktrees SET worktree=?2, tab_name=?3, branch=?4 WHERE worktree=?1",
            params![old_path, new_path, new_tab, new_branch],
        )?;
        Ok(())
    }

    /// Forget the registry row for a worktree group by its owning repo and tab
    /// name. This is intentionally independent of the worktree path so close /
    /// delete operations cannot be undone by a stale row whose path was moved or
    /// normalized differently than the live session group.
    pub fn del_worktree_for_tab(&self, repo_root: &str, tab: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM worktrees WHERE repo_path=?1 AND tab_name=?2",
            params![repo_root, tab],
        )?;
        Ok(())
    }

    pub fn set_worktree_sandbox(&self, wt: &str, backend: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE worktrees SET sandbox_backend=?2 WHERE worktree=?1",
            params![wt, backend],
        )?;
        Ok(())
    }

    pub fn worktree_sandbox(&self, wt: &str) -> Result<Option<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT sandbox_backend FROM worktrees WHERE worktree=?1")?;
        let mut rows = stmt.query(params![wt])?;
        if let Some(row) = rows.next()? {
            let val: Option<String> = row.get(0)?;
            Ok(val)
        } else {
            Ok(None)
        }
    }

    // --- container_events (sandbox audit trail) ------------------------------

    /// Record a sandbox event (exec, network, dns, orphan_gc) in the audit log.
    pub fn insert_container_event(
        &self,
        worktree: &str,
        ts: i64,
        kind: &str,
        detail: Option<&str>,
        exit_code: Option<i64>,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO container_events (worktree, ts, kind, detail, exit_code)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![worktree, ts, kind, detail, exit_code],
        )?;
        Ok(())
    }

    /// Retrieve the most recent `limit` container events for a worktree,
    /// newest first.
    pub fn container_events(&self, worktree: &str, limit: usize) -> Result<Vec<ContainerEvent>> {
        let mut stmt = self.conn.prepare(
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
    pub fn prune_container_events(&self, older_than_secs: i64) -> Result<usize> {
        let cutoff = crate::util::now() - older_than_secs;
        let n = self.conn.execute(
            "DELETE FROM container_events WHERE ts < ?1",
            params![cutoff],
        )?;
        Ok(n)
    }

    /// The worktree path for a (session, tab) pair — how the panel plugin maps
    /// the focused tab to a worktree (PaneInfo carries no cwd).
    pub fn worktree_for_tab(&self, session: &str, tab: &str) -> Result<Option<String>> {
        let r = self
            .conn
            .query_row(
                "SELECT worktree FROM worktrees WHERE session_name=?1 AND tab_name=?2 LIMIT 1",
                params![session, tab],
                |r| r.get::<_, String>(0),
            )
            .ok();
        Ok(r)
    }

    /// All recorded worktrees (metadata only; git supplies live status).
    pub fn worktrees(&self) -> Result<Vec<WorktreeRow>> {
        // `position` is the persistent sort key (creation order by default,
        // user-reorderable). Order by it so every consumer — the sidebar's
        // unloaded-workspace rows and the resurrect adopt loop — is stable;
        // created_at/path are deterministic tie-breakers for any unset row.
        let mut stmt = self.conn.prepare(
            "SELECT worktree, branch, agent, created_at, repo_path, tab_name, session_name, location, position
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
            })
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// Swap the persisted sort positions of two worktrees (by path). Used by
    /// the sidebar's manual reorder (Shift+Alt+↑/↓): the caller picks the two
    /// adjacent siblings, this exchanges their `position` so the new order
    /// survives restart. Positions are globally unique (migration + MAX+1
    /// inserts), so a swap can never create a collision.
    pub fn swap_worktree_positions(&self, a: &str, b: &str) -> Result<()> {
        // Read both first, then write — a single CASE-UPDATE that reads the
        // table it mutates can observe its own intermediate write and clobber
        // the swap.
        let pos = |wt: &str| -> Result<Option<i64>> {
            Ok(self
                .conn
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
    pub fn set_worktree_position(&self, wt: &str, position: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE worktrees SET position=?2 WHERE worktree=?1",
            params![wt, position],
        )?;
        Ok(())
    }

    // --- v6 session/layout persistence (native-host resurrect) -------------

    /// Insert or replace a worktree group's persisted row.
    pub fn put_tab_group(&self, session: &str, row: &crate::models::TabGroupRow) -> Result<()> {
        self.conn.execute(
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
    pub fn put_group_tab(&self, session: &str, row: &crate::models::GroupTabRow) -> Result<()> {
        self.conn.execute(
            "INSERT INTO group_tabs
               (session_name, group_name, ordinal, title, pane_tree, focused_pane)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(session_name, group_name, ordinal) DO UPDATE SET
               title=?4, pane_tree=?5, focused_pane=?6",
            params![
                session,
                row.group_name,
                row.ordinal,
                row.title,
                row.pane_tree,
                row.focused_pane,
            ],
        )?;
        Ok(())
    }

    /// All persisted worktree groups for a session, in display order.
    pub fn groups_for_session(&self, session: &str) -> Result<Vec<crate::models::TabGroupRow>> {
        let mut stmt = self.conn.prepare(
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
    pub fn group_tabs_for_session(&self, session: &str) -> Result<Vec<crate::models::GroupTabRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT group_name, ordinal, title, pane_tree, focused_pane
               FROM group_tabs WHERE session_name=?1 ORDER BY group_name, ordinal",
        )?;
        let rows = stmt.query_map(params![session], |r| {
            Ok(crate::models::GroupTabRow {
                group_name: r.get(0)?,
                ordinal: r.get::<_, Option<i64>>(1)?.unwrap_or(0),
                title: r.get::<_, Option<String>>(2)?.unwrap_or_default(),
                pane_tree: r.get::<_, Option<String>>(3)?.unwrap_or_default(),
                focused_pane: r.get::<_, Option<i64>>(4)?.unwrap_or(0),
            })
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// Forget one worktree group and its tabs (on worktree close).
    pub fn delete_tab_group(&self, session: &str, name: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM group_tabs WHERE session_name=?1 AND group_name=?2",
            params![session, name],
        )?;
        self.conn.execute(
            "DELETE FROM tab_groups WHERE session_name=?1 AND name=?2",
            params![session, name],
        )?;
        Ok(())
    }

    /// Wipe a session's whole persisted layout (groups + tabs). The host
    /// persists snapshots as clear-then-insert inside one transaction so
    /// closed/renamed entries can't linger.
    pub fn clear_session_layout(&self, session: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM group_tabs WHERE session_name=?1",
            params![session],
        )?;
        self.conn.execute(
            "DELETE FROM tab_groups WHERE session_name=?1",
            params![session],
        )?;
        Ok(())
    }

    /// Record which worktree group is active (for restoring focus on resurrect).
    pub fn set_active_tab(&self, session: &str, tab: &str, now: i64) -> Result<()> {
        self.conn.execute(
            "INSERT INTO session_state (session_name, active_tab, updated_at)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(session_name) DO UPDATE SET active_tab=?2, updated_at=?3",
            params![session, tab, now],
        )?;
        Ok(())
    }

    /// The tab that was active at exit, if recorded.
    pub fn active_tab(&self, session: &str) -> Result<Option<String>> {
        let r = self
            .conn
            .query_row(
                "SELECT active_tab FROM session_state WHERE session_name=?1",
                params![session],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?;
        Ok(r.flatten())
    }

    // --- ui_state (sidebar view state: collapse, sort, width, pins) ---------

    /// Read a persisted UI-state value, `None` if unset.
    pub fn get_ui_state(&self, scope: &str, key: &str) -> Result<Option<String>> {
        let r = self
            .conn
            .query_row(
                "SELECT value FROM ui_state WHERE scope=?1 AND key=?2",
                params![scope, key],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?;
        Ok(r.flatten())
    }

    /// Upsert a persisted UI-state value for `(scope, key)`.
    pub fn set_ui_state(&self, scope: &str, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO ui_state (scope, key, value)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(scope, key) DO UPDATE SET value=?3",
            params![scope, key, value],
        )?;
        Ok(())
    }

    /// Delete a persisted UI-state value (e.g. unpinning). No-op if absent.
    pub fn del_ui_state(&self, scope: &str, key: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM ui_state WHERE scope=?1 AND key=?2",
            params![scope, key],
        )?;
        Ok(())
    }

    /// All `(key, value)` pairs in a scope — used to load every collapse/pin
    /// entry at once on sidebar build.
    pub fn ui_state_in_scope(&self, scope: &str) -> Result<Vec<(String, String)>> {
        let mut stmt = self
            .conn
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
    pub fn set_pin_state(&self, session: &str, json: &str, now: i64) -> Result<()> {
        self.conn.execute(
            "INSERT INTO session_state (session_name, pin_state, updated_at)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(session_name) DO UPDATE SET pin_state=?2, updated_at=?3",
            params![session, json, now],
        )?;
        Ok(())
    }

    /// The running-pin JSON recorded for a session, if any.
    pub fn pin_state(&self, session: &str) -> Result<Option<String>> {
        let r = self
            .conn
            .query_row(
                "SELECT pin_state FROM session_state WHERE session_name=?1",
                params![session],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?;
        Ok(r.flatten())
    }
}

/// Split a legacy v4/v5 tab name into its worktree-group base and page number:
/// `"app/feat ·3"` → `("app/feat", Some(3))`, `"app/feat"` → `("app/feat", None)`.
fn split_page_suffix(name: &str) -> (&str, Option<u32>) {
    if let Some((base, page)) = name.rsplit_once(" ·")
        && !base.is_empty()
        && let Ok(n) = page.parse::<u32>()
    {
        return (base, Some(n));
    }
    (name, None)
}

/// v5 → v6: transform the flat `tab_layout` (one row per worktree, extra pages
/// as " ·N" name suffixes) into `tab_groups` + `group_tabs`, remap each
/// session's `session_state.active_tab` from a tab name to its group name, and
/// drop the legacy table. Runs in one transaction; on failure the legacy table
/// (and the old active markers) survive untouched and the host boots with a
/// fresh layout — the next open retries.
fn migrate_tab_layout_v6(conn: &Connection) {
    let has_legacy = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='tab_layout'",
            [],
            |_| Ok(()),
        )
        .optional()
        .ok()
        .flatten()
        .is_some();
    if !has_legacy {
        return;
    }
    let run = || -> Result<()> {
        let tx = conn.unchecked_transaction()?;
        struct Legacy {
            session: String,
            name: String,
            kind: String,
            worktree: String,
            pane_tree: String,
            focused: i64,
        }
        let legacy: Vec<Legacy> = {
            let mut stmt = tx.prepare(
                "SELECT session_name, tab_name, kind, worktree, pane_tree, focused_pane
                   FROM tab_layout ORDER BY session_name, ordinal",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok(Legacy {
                    session: r.get::<_, Option<String>>(0)?.unwrap_or_default(),
                    name: r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                    kind: r.get::<_, Option<String>>(2)?.unwrap_or_default(),
                    worktree: r.get::<_, Option<String>>(3)?.unwrap_or_default(),
                    pane_tree: r.get::<_, Option<String>>(4)?.unwrap_or_default(),
                    focused: r.get::<_, Option<i64>>(5)?.unwrap_or(0),
                })
            })?;
            rows.filter_map(|r| r.ok()).collect()
        };

        // Group rows by (session, base name) preserving first-seen order; track
        // each tab's original full name so active markers can be remapped.
        struct Group {
            session: String,
            name: String,
            kind: String,
            worktree: String,
            tabs: Vec<(String, String, i64)>, // (orig full name, pane_tree, focused)
        }
        let mut groups: Vec<Group> = Vec::new();
        for row in legacy {
            if row.name.is_empty() {
                continue;
            }
            let (base, _) = split_page_suffix(&row.name);
            let kind = if row.kind == "home" { "home" } else { "branch" };
            let g = match groups
                .iter_mut()
                .find(|g| g.session == row.session && g.name == base)
            {
                Some(g) => g,
                None => {
                    groups.push(Group {
                        session: row.session.clone(),
                        name: base.to_string(),
                        kind: kind.to_string(),
                        worktree: String::new(),
                        tabs: Vec::new(),
                    });
                    groups.last_mut().expect("just pushed")
                }
            };
            if g.worktree.is_empty() && !row.worktree.is_empty() {
                g.worktree = row.worktree.clone();
            }
            g.tabs.push((row.name, row.pane_tree, row.focused));
        }

        let mut ordinal_in: std::collections::HashMap<String, i64> = Default::default();
        for g in &groups {
            let ord = ordinal_in.entry(g.session.clone()).or_insert(0);
            // The group's active tab: the session's recorded active tab name if
            // it lives in this group, else the first tab.
            let active_name: Option<String> = tx
                .query_row(
                    "SELECT active_tab FROM session_state WHERE session_name=?1",
                    params![g.session],
                    |r| r.get::<_, Option<String>>(0),
                )
                .optional()?
                .flatten();
            let active_idx = active_name
                .as_deref()
                .and_then(|an| g.tabs.iter().position(|(orig, _, _)| orig == an))
                .unwrap_or(0) as i64;
            tx.execute(
                "INSERT OR REPLACE INTO tab_groups
                   (session_name, name, kind, worktree, ordinal, active_tab)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![g.session, g.name, g.kind, g.worktree, *ord, active_idx],
            )?;
            *ord += 1;
            for (i, (_, pane_tree, focused)) in g.tabs.iter().enumerate() {
                tx.execute(
                    "INSERT OR REPLACE INTO group_tabs
                       (session_name, group_name, ordinal, title, pane_tree, focused_pane)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        g.session,
                        g.name,
                        i as i64,
                        (i + 1).to_string(),
                        pane_tree,
                        focused
                    ],
                )?;
            }
            // Remap the session's active marker from tab name to group name.
            if let Some(an) = active_name.as_deref()
                && g.tabs.iter().any(|(orig, _, _)| orig == an)
            {
                tx.execute(
                    "UPDATE session_state SET active_tab=?2 WHERE session_name=?1",
                    params![g.session, g.name],
                )?;
            }
        }
        tx.execute("DROP TABLE tab_layout", [])?;
        tx.commit()?;
        Ok(())
    };
    if let Err(e) = run() {
        tracing::warn!(target: "superzej::db", error = %e, "v6 tab_layout migration failed; keeping legacy table");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Db {
        Db::open_memory().unwrap()
    }

    #[test]
    fn commit_cache_roundtrips_json_and_timestamp() {
        let db = db();
        assert!(db.get_commit_cache("/wt").unwrap().is_none());
        db.put_commit_cache("/wt", r#"[{"short":"abc1234"}]"#)
            .unwrap();
        let (json, fetched_at) = db.get_commit_cache("/wt").unwrap().unwrap();
        assert_eq!(json, r#"[{"short":"abc1234"}]"#);
        assert!(fetched_at > 0);
    }

    #[test]
    fn transaction_commits_on_ok_and_passes_value_through() {
        let db = db();
        let n = db
            .transaction(|db| {
                db.touch_repo("/r/a", "a")?;
                db.touch_repo("/r/b", "b")?;
                Ok(42)
            })
            .unwrap();
        assert_eq!(n, 42);
        assert_eq!(db.recent_repos(10).unwrap().len(), 2);
    }

    #[test]
    fn transaction_rolls_back_on_err() {
        let db = db();
        let res: Result<()> = db.transaction(|db| {
            db.touch_repo("/r/a", "a")?;
            anyhow::bail!("boom")
        });
        assert!(res.is_err());
        // The insert before the error must not be visible.
        assert!(db.recent_repos(10).unwrap().is_empty());
    }

    #[test]
    fn tab_groups_roundtrip_ordered_by_ordinal() {
        use crate::models::{GroupTabRow, TabGroupRow};
        let db = db();
        let sess = "s1";
        let mk = |name: &str, ord: i64| TabGroupRow {
            name: name.into(),
            kind: "branch".into(),
            worktree: format!("/wt/{name}"),
            ordinal: ord,
            active_tab: 0,
        };
        let mktab = |group: &str, ord: i64| GroupTabRow {
            group_name: group.into(),
            ordinal: ord,
            title: (ord + 1).to_string(),
            pane_tree: r#"{"leaf":0}"#.into(),
            focused_pane: 0,
        };
        // Insert out of order; expect ordinal ordering back.
        db.put_tab_group(sess, &mk("app/feat", 1)).unwrap();
        db.put_tab_group(sess, &mk("app/home", 0)).unwrap();
        db.put_group_tab(sess, &mktab("app/feat", 0)).unwrap();
        db.put_group_tab(sess, &mktab("app/feat", 1)).unwrap();
        db.put_group_tab(sess, &mktab("app/home", 0)).unwrap();
        let groups = db.groups_for_session(sess).unwrap();
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].name, "app/home");
        assert_eq!(groups[1].name, "app/feat");
        let tabs = db.group_tabs_for_session(sess).unwrap();
        assert_eq!(tabs.len(), 3);

        // Upsert replaces in place (no duplicate row).
        db.put_tab_group(sess, &mk("app/feat", 5)).unwrap();
        let groups = db.groups_for_session(sess).unwrap();
        assert_eq!(groups.len(), 2);
        assert_eq!(
            groups
                .iter()
                .find(|g| g.name == "app/feat")
                .unwrap()
                .ordinal,
            5
        );

        // Delete removes the group and its tabs; other session is untouched.
        db.put_tab_group("other", &mk("x/home", 0)).unwrap();
        db.put_group_tab("other", &mktab("x/home", 0)).unwrap();
        db.delete_tab_group(sess, "app/feat").unwrap();
        assert_eq!(db.groups_for_session(sess).unwrap().len(), 1);
        assert_eq!(db.group_tabs_for_session(sess).unwrap().len(), 1);
        assert_eq!(db.groups_for_session("other").unwrap().len(), 1);

        // clear_session_layout wipes one session only.
        db.clear_session_layout(sess).unwrap();
        assert!(db.groups_for_session(sess).unwrap().is_empty());
        assert!(db.group_tabs_for_session(sess).unwrap().is_empty());
        assert_eq!(db.groups_for_session("other").unwrap().len(), 1);
    }

    #[test]
    fn split_page_suffix_cases() {
        assert_eq!(split_page_suffix("app/feat"), ("app/feat", None));
        assert_eq!(split_page_suffix("app/feat ·2"), ("app/feat", Some(2)));
        assert_eq!(split_page_suffix("app/feat ·x"), ("app/feat ·x", None));
        assert_eq!(split_page_suffix(" ·2"), (" ·2", None));
    }

    /// Build a legacy v5 DB file by hand (raw SQL, no Db API), then open it via
    /// `Db::open_at` and assert the v6 transform.
    #[test]
    fn migrates_v5_tab_layout_into_groups() {
        let dir = std::env::temp_dir().join(format!("sz-db-mig-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("db.sqlite");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                r#"
                PRAGMA user_version = 5;
                CREATE TABLE tab_layout (
                  session_name TEXT, tab_name TEXT, kind TEXT, worktree TEXT,
                  pane_tree TEXT, ordinal INTEGER, focused_pane INTEGER,
                  PRIMARY KEY (session_name, tab_name));
                CREATE TABLE session_state (
                  session_name TEXT PRIMARY KEY, active_tab TEXT, updated_at INTEGER);
                INSERT INTO tab_layout VALUES
                  ('/r', 'app/home',    'home',     '/r',        '{"leaf":0}', 0, 0),
                  ('/r', 'app/feat',    'worktree', '/wt/feat',  '{"leaf":1}', 1, 1),
                  ('/r', 'app/feat ·2', 'worktree', '/wt/feat',  '{"leaf":2}', 2, 2),
                  ('/r', 'scratch',     'extra',    '',          '{"leaf":3}', 3, 0),
                  ('/q', 'q/home',      'home',     '/q',        '{"leaf":0}', 0, 0);
                INSERT INTO session_state VALUES ('/r', 'app/feat ·2', 1);
                "#,
            )
            .unwrap();
        }
        let db = Db::open_at(&path).unwrap();

        // Legacy table is gone; groups exist per base name.
        let groups = db.groups_for_session("/r").unwrap();
        assert_eq!(
            groups.iter().map(|g| g.name.as_str()).collect::<Vec<_>>(),
            vec!["app/home", "app/feat", "scratch"]
        );
        let feat = groups.iter().find(|g| g.name == "app/feat").unwrap();
        assert_eq!(feat.kind, "branch");
        assert_eq!(feat.worktree, "/wt/feat");
        assert_eq!(feat.active_tab, 1, "active page ·2 became tab index 1");
        assert_eq!(groups[0].kind, "home");

        let tabs = db.group_tabs_for_session("/r").unwrap();
        let feat_tabs: Vec<_> = tabs.iter().filter(|t| t.group_name == "app/feat").collect();
        assert_eq!(feat_tabs.len(), 2);
        assert_eq!(feat_tabs[0].title, "1");
        assert_eq!(feat_tabs[0].pane_tree, r#"{"leaf":1}"#);
        assert_eq!(feat_tabs[1].pane_tree, r#"{"leaf":2}"#);
        assert_eq!(feat_tabs[1].focused_pane, 2);

        // The session's active marker now names the group.
        assert_eq!(db.active_tab("/r").unwrap().as_deref(), Some("app/feat"));
        // The untouched session migrated too.
        assert_eq!(db.groups_for_session("/q").unwrap().len(), 1);

        // Re-open: migration is idempotent (legacy table is gone).
        drop(db);
        let db = Db::open_at(&path).unwrap();
        assert_eq!(db.groups_for_session("/r").unwrap().len(), 3);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn active_tab_persists_per_session() {
        let db = db();
        assert_eq!(db.active_tab("s").unwrap(), None);
        db.set_active_tab("s", "app/feat", 100).unwrap();
        assert_eq!(db.active_tab("s").unwrap().as_deref(), Some("app/feat"));
        // Upsert moves it.
        db.set_active_tab("s", "app/home", 200).unwrap();
        assert_eq!(db.active_tab("s").unwrap().as_deref(), Some("app/home"));
    }

    #[test]
    fn pin_state_persists_without_clobbering_active_tab() {
        let db = db();
        assert_eq!(db.pin_state("s").unwrap(), None);
        // active_tab and pin_state coexist in the same row, set independently.
        db.set_active_tab("s", "app/home", 10).unwrap();
        db.set_pin_state("s", r#"[{"name":"mail","placement":"float"}]"#, 20)
            .unwrap();
        assert_eq!(db.active_tab("s").unwrap().as_deref(), Some("app/home"));
        assert_eq!(
            db.pin_state("s").unwrap().as_deref(),
            Some(r#"[{"name":"mail","placement":"float"}]"#)
        );
        // Updating pin_state leaves active_tab intact.
        db.set_pin_state("s", "[]", 30).unwrap();
        assert_eq!(db.active_tab("s").unwrap().as_deref(), Some("app/home"));
        assert_eq!(db.pin_state("s").unwrap().as_deref(), Some("[]"));
    }

    #[test]
    fn palette_usage_accumulates_and_reports() {
        let db = db();
        assert!(db.palette_usage().unwrap().is_empty());
        // First bump inserts; subsequent bumps increment the count in place.
        db.bump_palette_usage("new-worktree").unwrap();
        db.bump_palette_usage("new-worktree").unwrap();
        db.bump_palette_usage("diff").unwrap();
        let usage = db.palette_usage().unwrap();
        assert_eq!(usage.len(), 2, "one row per distinct key");
        let by_key: std::collections::HashMap<_, _> = usage
            .iter()
            .map(|(k, c, l)| (k.as_str(), (*c, *l)))
            .collect();
        assert_eq!(
            by_key["new-worktree"].0, 2,
            "repeated bump increments count"
        );
        assert_eq!(by_key["diff"].0, 1);
        // last_used is stamped (non-zero) on every key.
        assert!(by_key["new-worktree"].1 > 0 && by_key["diff"].1 > 0);
    }

    #[test]
    fn repos_recents_order_by_seq() {
        let db = db();
        db.touch_repo("/a", "a").unwrap();
        db.touch_repo("/b", "b").unwrap();
        db.touch_repo("/a", "a").unwrap(); // re-open bumps seq
        let recents = db.recent_repos(10).unwrap();
        assert_eq!(recents, vec!["/a".to_string(), "/b".to_string()]);
        assert!(db.recent_repos(1).unwrap().len() == 1);
        assert!(db.is_known_repo("/a").unwrap());
        assert!(!db.is_known_repo("/nope").unwrap());
        assert!(db.known_repos().unwrap().contains(&"/b".to_string()));
    }

    #[test]
    fn workspaces_roundtrip() {
        let db = db();
        db.put_workspace("/repo", "repo", "repo").unwrap();
        db.put_workspace("/repo", "repo2", "repo").unwrap(); // upsert renames
        let ws = db.workspaces().unwrap();
        assert_eq!(ws.len(), 1);
        assert_eq!(ws[0].repo_path, "/repo");
        assert_eq!(ws[0].name, "repo2");
        assert_eq!(ws[0].kind, "repo");
        assert!(db.is_known_repo("/repo").unwrap());
    }

    #[test]
    fn workspace_kind_is_insert_only() {
        let db = db();
        db.put_workspace("/d", "d", "dir").unwrap();
        // A later refresh passing "repo" must not downgrade an existing dir.
        db.put_workspace("/d", "d", "repo").unwrap();
        assert_eq!(db.workspaces().unwrap()[0].kind, "dir");
    }

    #[test]
    fn slug_reuse_and_collision_suffix() {
        let db = db();
        // First repo takes the bare base.
        assert_eq!(db.slug_for_repo("/x/app", "app").unwrap(), "app");
        // Same repo reuses its slug.
        assert_eq!(db.slug_for_repo("/x/app", "app").unwrap(), "app");
        // Different repo, same basename → suffixed.
        assert_eq!(db.slug_for_repo("/y/app", "app").unwrap(), "app-2");
        assert_eq!(db.slug_for_repo("/z/app", "app").unwrap(), "app-3");
    }

    #[test]
    fn pr_branch_cache_roundtrip_and_upsert() {
        let db = db();
        assert!(db.get_pr_branch_cache("/repo").unwrap().is_none());
        db.put_pr_branch_cache("/repo", "[{\"number\":1}]").unwrap();
        let (json, at) = db.get_pr_branch_cache("/repo").unwrap().unwrap();
        assert_eq!(json, "[{\"number\":1}]");
        assert!(at > 0);
        db.put_pr_branch_cache("/repo", "[]").unwrap();
        assert_eq!(db.get_pr_branch_cache("/repo").unwrap().unwrap().0, "[]");
    }

    #[test]
    fn open_pr_counts_by_branch_counts_only_open_prs() {
        let db = db();
        // No cache yet → empty map.
        assert!(db.get_open_pr_counts_by_branch("/repo").unwrap().is_empty());

        // Two open PRs on `feat`, one merged on `feat`, one open on `fix`.
        let json = r#"[
            {"number":1,"headRefName":"feat","state":"OPEN","url":"u1","isDraft":false},
            {"number":2,"headRefName":"feat","state":"OPEN","url":"u2","isDraft":false},
            {"number":3,"headRefName":"feat","state":"MERGED","url":"u3","isDraft":false},
            {"number":4,"headRefName":"fix","state":"OPEN","url":"u4","isDraft":false}
        ]"#;
        db.put_pr_branch_cache("/repo", json).unwrap();
        let counts = db.get_open_pr_counts_by_branch("/repo").unwrap();
        assert_eq!(counts.get("feat"), Some(&2), "two OPEN PRs on feat");
        assert_eq!(counts.get("fix"), Some(&1), "one OPEN PR on fix");
        assert_eq!(counts.len(), 2, "merged/closed PRs are excluded");
    }

    #[test]
    fn open_pr_counts_by_branch_handles_garbled_cache() {
        let db = db();
        db.put_pr_branch_cache("/repo", "not json").unwrap();
        assert!(db.get_open_pr_counts_by_branch("/repo").unwrap().is_empty());
    }

    #[test]
    fn undo_marks_record_dedupe_and_cap() {
        let db = db();
        assert!(db.undo_marks("/wt").unwrap().is_empty());
        db.add_undo_mark("/wt", "aaa").unwrap();
        db.add_undo_mark("/wt", "bbb").unwrap();
        db.add_undo_mark("/wt", "aaa").unwrap(); // refresh, not duplicate
        let marks = db.undo_marks("/wt").unwrap();
        assert_eq!(marks.len(), 2);
        // Other worktrees are isolated.
        assert!(db.undo_marks("/other").unwrap().is_empty());
        // Cap: 110 inserts keep only the freshest 100.
        for i in 0..110 {
            db.add_undo_mark("/cap", &format!("sha{i:03}")).unwrap();
        }
        assert_eq!(db.undo_marks("/cap").unwrap().len(), 100);
    }

    #[test]
    fn pr_and_diff_caches() {
        let db = db();
        assert!(db.get_pr_cache("/wt").unwrap().is_none());
        db.put_pr_cache("/wt", "br", "{\"k\":1}").unwrap();
        let (json, at) = db.get_pr_cache("/wt").unwrap().unwrap();
        assert_eq!(json, "{\"k\":1}");
        assert!(at > 0);
        db.put_pr_cache("/wt", "br", "{\"k\":2}").unwrap(); // upsert
        assert_eq!(db.get_pr_cache("/wt").unwrap().unwrap().0, "{\"k\":2}");

        assert!(db.get_diff_cache("/wt").unwrap().is_none());
        db.put_diff_cache("/wt", "M\tfile.rs").unwrap();
        assert_eq!(db.get_diff_cache("/wt").unwrap().unwrap().0, "M\tfile.rs");

        assert!(db.get_test_cache("/wt").unwrap().is_none());
        db.put_test_cache("/wt", "{\"summary\":\"ok\"}").unwrap();
        assert_eq!(
            db.get_test_cache("/wt").unwrap().unwrap().0,
            "{\"summary\":\"ok\"}"
        );
        db.put_test_cache("/wt", "{\"summary\":\"fail\"}").unwrap();
        assert_eq!(
            db.get_test_cache("/wt").unwrap().unwrap().0,
            "{\"summary\":\"fail\"}"
        );

        // loc cache: miss → insert → upsert.
        assert!(db.get_loc_cache("/wt").unwrap().is_none());
        db.put_loc_cache("/wt", 123).unwrap();
        assert_eq!(db.get_loc_cache("/wt").unwrap(), Some(123));
        db.put_loc_cache("/wt", 456).unwrap();
        assert_eq!(db.get_loc_cache("/wt").unwrap(), Some(456));
    }

    #[test]
    fn worktree_crud() {
        let db = db();
        db.put_worktree("app/feat", "/x/app", "/wt/feat", "sz/feat", None)
            .unwrap();

        db.set_worktree_sandbox("/wt/feat", "podman").unwrap();
        let sb = db.worktree_sandbox("/wt/feat").unwrap();
        assert_eq!(sb, Some("podman".to_string()));

        // metadata round-trips
        let all = db.worktrees().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].worktree, "/wt/feat");
        assert_eq!(all[0].branch, "sz/feat");
        assert_eq!(all[0].repo_root, "/x/app");
        // tab → worktree mapping uses the recorded session.
        let sess = session();
        assert_eq!(
            db.worktree_for_tab(&sess, "app/feat").unwrap().as_deref(),
            Some("/wt/feat")
        );
        assert_eq!(
            db.repo_root_for("/wt/feat").unwrap().as_deref(),
            Some("/x/app")
        );
        // agent: empty → None, then set → Some.
        assert!(db.worktree_agent("/wt/feat").unwrap().is_none());
        db.set_worktree_agent("/wt/feat", "claude").unwrap();
        assert_eq!(
            db.worktree_agent("/wt/feat").unwrap().as_deref(),
            Some("claude")
        );
        // location: none by default; set via upsert.
        assert!(
            db.location_for("/wt/feat")
                .unwrap()
                .map(|s| s.is_empty())
                .unwrap_or(true)
        );
        db.put_worktree(
            "app/feat",
            "/x/app",
            "/wt/feat-renamed-on-disk",
            "sz/feat",
            Some("{\"host\":\"box\"}"),
        )
        .unwrap();
        db.del_worktree_for_tab("/x/app", "app/feat").unwrap();
        assert!(
            db.worktrees().unwrap().is_empty(),
            "closing/deleting a worktree group must forget registry rows even if the path changed"
        );

        db.put_worktree("app/other", "/x/app", "/wt/other", "sz/other", None)
            .unwrap();
        // delete
        db.del_worktree("/wt/other").unwrap();
        assert!(db.worktrees().unwrap().is_empty());
    }

    #[test]
    fn rename_worktree_rekeys_path_tab_and_branch() {
        let db = db();
        db.put_worktree("app/old", "/x/app", "/wt/old", "old", None)
            .unwrap();
        db.set_worktree_position("/wt/old", 7).unwrap();
        db.rename_worktree("/wt/old", "/wt/new", "app/new", "new")
            .unwrap();
        let rows = db.worktrees().unwrap();
        assert_eq!(rows.len(), 1);
        let w = &rows[0];
        assert_eq!(w.worktree, "/wt/new");
        assert_eq!(w.tab_name, "app/new");
        assert_eq!(w.branch, "new");
        assert_eq!(w.position, 7, "position is preserved across rename");
        // Renaming a missing row is a no-op (no panic, no insert).
        db.rename_worktree("/wt/missing", "/wt/x", "app/x", "x")
            .unwrap();
        assert_eq!(db.worktrees().unwrap().len(), 1);
    }

    #[test]
    fn worktree_position_default_is_creation_order() {
        let db = db();
        // Inserted a, b, c — `worktrees()` returns them in that creation order
        // regardless of branch name (no alphabetizing), and positions are the
        // dense 0,1,2 the appending MAX+1 insert assigns.
        db.put_worktree("app/c", "/x/app", "/wt/c", "sz/c", None)
            .unwrap();
        db.put_worktree("app/a", "/x/app", "/wt/a", "sz/a", None)
            .unwrap();
        db.put_worktree("app/b", "/x/app", "/wt/b", "sz/b", None)
            .unwrap();
        let order: Vec<_> = db
            .worktrees()
            .unwrap()
            .into_iter()
            .map(|w| (w.worktree, w.position))
            .collect();
        assert_eq!(
            order,
            vec![
                ("/wt/c".into(), 0),
                ("/wt/a".into(), 1),
                ("/wt/b".into(), 2),
            ]
        );

        // Re-registering an existing worktree (upsert) keeps its position — a
        // metadata refresh must never reshuffle the list.
        db.put_worktree("app/c", "/x/app", "/wt/c", "sz/c-renamed", None)
            .unwrap();
        let pos_c = db
            .worktrees()
            .unwrap()
            .into_iter()
            .find(|w| w.worktree == "/wt/c")
            .unwrap()
            .position;
        assert_eq!(pos_c, 0, "upsert must preserve position");
    }

    #[test]
    fn swap_worktree_positions_reorders() {
        let db = db();
        db.put_worktree("app/a", "/x/app", "/wt/a", "sz/a", None)
            .unwrap();
        db.put_worktree("app/b", "/x/app", "/wt/b", "sz/b", None)
            .unwrap();
        db.put_worktree("app/c", "/x/app", "/wt/c", "sz/c", None)
            .unwrap();

        // Swap the first two: order becomes b, a, c.
        db.swap_worktree_positions("/wt/a", "/wt/b").unwrap();
        let order: Vec<String> = db
            .worktrees()
            .unwrap()
            .into_iter()
            .map(|w| w.worktree)
            .collect();
        assert_eq!(order, vec!["/wt/b", "/wt/a", "/wt/c"]);

        // set_worktree_position is the persist-side primitive; moving c to the
        // front (a fresh min) floats it above the rest.
        db.set_worktree_position("/wt/c", -1).unwrap();
        let first = db.worktrees().unwrap().into_iter().next().unwrap().worktree;
        assert_eq!(first, "/wt/c");
    }

    #[test]
    fn empty_and_miss_paths() {
        let db = db();
        // Fresh DB: queries return empty / None rather than erroring.
        assert!(db.recent_repos(5).unwrap().is_empty());
        assert!(db.known_repos().unwrap().is_empty());
        assert!(db.workspaces().unwrap().is_empty());
        assert!(db.worktrees().unwrap().is_empty());
        assert!(db.worktree_for_tab("s", "t").unwrap().is_none());
        assert!(db.location_for("/missing").unwrap().is_none());
        assert!(db.repo_root_for("/missing").unwrap().is_none());
        assert!(db.worktree_agent("/missing").unwrap().is_none());
        assert!(!db.is_known_repo("/missing").unwrap());
        // session() honors the env (defaults to "default").
        assert!(!session().is_empty());
    }

    // Cover the real file-backed open() path (db_path + dir creation + on-disk
    // connection + migration) by pointing XDG_STATE_HOME at a temp dir.
    #[test]
    fn open_on_disk() {
        let dir =
            std::env::temp_dir().join(format!("sz-db-disk-{}-{:p}", std::process::id(), &0u8));
        let _ = std::fs::remove_dir_all(&dir);
        // Open at an explicit path rather than mutating the global XDG_STATE_HOME
        // (which other parallel tests read via Db::open()/db_path()).
        let path = dir.join("superzej/superzej.db");
        {
            let db = Db::open_at(&path).unwrap();
            db.touch_repo("/r", "r").unwrap();
            assert_eq!(db.recent_repos(5).unwrap(), vec!["/r".to_string()]);
        }
        // Reopen the persisted file: migration is idempotent, data survives.
        {
            let db = Db::open_at(&path).unwrap();
            assert!(db.is_known_repo("/r").unwrap());
        }
        let _ = std::fs::remove_dir_all(&dir);
        // db_path() still derives the default location from XDG_STATE_HOME.
        assert!(db_path().ends_with("superzej/superzej.db"));
    }

    #[test]
    fn ui_state_roundtrip_upsert_and_scope_isolation() {
        let db = db();
        // Unset reads as None.
        assert_eq!(db.get_ui_state("s1", "sort_mode").unwrap(), None);

        // Insert, then read back.
        db.set_ui_state("s1", "sort_mode", "name").unwrap();
        assert_eq!(
            db.get_ui_state("s1", "sort_mode").unwrap(),
            Some("name".to_string())
        );

        // Upsert replaces in place (no duplicate row).
        db.set_ui_state("s1", "sort_mode", "recent").unwrap();
        assert_eq!(
            db.get_ui_state("s1", "sort_mode").unwrap(),
            Some("recent".to_string())
        );

        // A different scope with the same key is isolated.
        db.set_ui_state("s2", "sort_mode", "activity").unwrap();
        assert_eq!(
            db.get_ui_state("s1", "sort_mode").unwrap(),
            Some("recent".to_string())
        );

        // Bulk read of a scope returns only that scope's keys.
        db.set_ui_state("s1", "collapse:app", "1").unwrap();
        let mut pairs = db.ui_state_in_scope("s1").unwrap();
        pairs.sort();
        assert_eq!(
            pairs,
            vec![
                ("collapse:app".to_string(), "1".to_string()),
                ("sort_mode".to_string(), "recent".to_string()),
            ]
        );

        // Delete removes just that key.
        db.del_ui_state("s1", "collapse:app").unwrap();
        assert_eq!(db.get_ui_state("s1", "collapse:app").unwrap(), None);
        assert_eq!(
            db.get_ui_state("s1", "sort_mode").unwrap(),
            Some("recent".to_string())
        );
    }

    #[test]
    fn issue_cache_roundtrips_and_updates() {
        let db = db();
        // Cold cache returns None.
        assert!(db.get_issue_cache("/repo", "linear").unwrap().is_none());
        // Write and read back.
        db.put_issue_cache("/repo", "linear", r#"[{"id":"linear:A-1"}]"#)
            .unwrap();
        let (json, ts) = db.get_issue_cache("/repo", "linear").unwrap().unwrap();
        assert_eq!(json, r#"[{"id":"linear:A-1"}]"#);
        assert!(ts > 0);
        // Different provider is independent.
        assert!(db.get_issue_cache("/repo", "github").unwrap().is_none());
        // Upsert overwrites.
        db.put_issue_cache("/repo", "linear", r#"[{"id":"linear:A-2"}]"#)
            .unwrap();
        let (json2, _) = db.get_issue_cache("/repo", "linear").unwrap().unwrap();
        assert_eq!(json2, r#"[{"id":"linear:A-2"}]"#);
    }

    #[test]
    fn issue_links_crud() {
        let db = db();
        // No links initially.
        assert!(db.linked_issues("/wt/a").unwrap().is_empty());
        // Link two issues.
        db.link_issue("/wt/a", "linear:A-1").unwrap();
        db.link_issue("/wt/a", "github:42").unwrap();
        let links = db.linked_issues("/wt/a").unwrap();
        assert_eq!(links.len(), 2);
        assert!(links.contains(&"linear:A-1".to_string()));
        assert!(links.contains(&"github:42".to_string()));
        // Another worktree is isolated.
        assert!(db.linked_issues("/wt/b").unwrap().is_empty());
        // Unlink removes exactly one.
        db.unlink_issue("/wt/a", "linear:A-1").unwrap();
        let links = db.linked_issues("/wt/a").unwrap();
        assert_eq!(links.len(), 1);
        assert_eq!(links[0], "github:42");
        // Linking twice is idempotent (no duplicate).
        db.link_issue("/wt/a", "github:42").unwrap();
        assert_eq!(db.linked_issues("/wt/a").unwrap().len(), 1);
    }

    #[test]
    fn notifications_put_and_read_and_mark_read() {
        let db = db();
        // No notifications initially.
        assert!(db.get_unread_notifications().unwrap().is_empty());
        // Add two notifications.
        db.put_notification("status_changed", "linear:A-1", "A-1 moved to Done", "/wt/x")
            .unwrap();
        db.put_notification("assigned", "linear:A-2", "A-2 assigned to you", "/wt/x")
            .unwrap();
        let unread = db.get_unread_notifications().unwrap();
        assert_eq!(unread.len(), 2);
        // Mark one read by id.
        let first_id = unread[0].id;
        db.mark_notification_read(first_id).unwrap();
        assert_eq!(db.get_unread_notifications().unwrap().len(), 1);
        // Mark all read clears the rest.
        db.mark_all_notifications_read().unwrap();
        assert!(db.get_unread_notifications().unwrap().is_empty());
    }

    #[test]
    fn agent_dispatch_roundtrip() {
        let db = db();
        // No dispatch for unknown path.
        assert!(db.dispatch_for_worktree("/wt/issue").unwrap().is_none());
        // Insert a dispatch.
        let id = db
            .put_agent_dispatch("linear:A-1", "/wt/issue", "claude")
            .unwrap();
        assert!(id > 0);
        // Retrieve by worktree path.
        let found = db.dispatch_for_worktree("/wt/issue").unwrap();
        assert_eq!(found, Some(id));
        // Update status.
        db.update_dispatch_status(id, "running").unwrap();
        // A different worktree is isolated.
        assert!(db.dispatch_for_worktree("/wt/other").unwrap().is_none());
    }

    #[test]
    fn dispatch_info_for_worktree_returns_id_and_issue_id() {
        let db = db();
        // No result for unknown path.
        assert!(db.dispatch_info_for_worktree("/wt/x").unwrap().is_none());
        // Insert dispatch.
        let id = db
            .put_agent_dispatch("linear:B-7", "/wt/x", "claude")
            .unwrap();
        // Info returns both id and issue id.
        let info = db.dispatch_info_for_worktree("/wt/x").unwrap();
        assert_eq!(info, Some((id, "linear:B-7".to_string())));
        // Multiple dispatches: most recent wins.
        let id2 = db
            .put_agent_dispatch("linear:B-8", "/wt/x", "claude")
            .unwrap();
        let info2 = db.dispatch_info_for_worktree("/wt/x").unwrap();
        assert_eq!(info2, Some((id2, "linear:B-8".to_string())));
    }

    #[test]
    fn get_all_notifications_returns_read_and_unread() {
        let db = db();
        // 2 read + 1 unread.
        let id1 = db
            .put_notification("assigned", "linear:A-1", "msg1", "/wt")
            .unwrap();
        let id2 = db
            .put_notification("status_changed", "linear:A-2", "msg2", "/wt")
            .unwrap();
        db.put_notification("test_failed", "/wt", "msg3", "/wt")
            .unwrap();
        db.mark_notification_read(id1).unwrap();
        db.mark_notification_read(id2).unwrap();
        // get_all_notifications returns all 3.
        let all = db.get_all_notifications(100).unwrap();
        assert_eq!(all.len(), 3);
        // get_unread_notifications returns only 1.
        let unread = db.get_unread_notifications().unwrap();
        assert_eq!(unread.len(), 1);
    }

    #[test]
    fn get_all_notifications_respects_limit() {
        let db = db();
        for i in 0..60 {
            db.put_notification("assigned", &format!("ref:{i}"), "msg", "/wt")
                .unwrap();
        }
        let capped = db.get_all_notifications(50).unwrap();
        assert_eq!(capped.len(), 50);
        let all = db.get_all_notifications(100).unwrap();
        assert_eq!(all.len(), 60);
    }

    #[test]
    fn delete_notification_removes_single_row() {
        let db = db();
        let id = db
            .put_notification("agent_done", "linear:A-1", "done", "/wt")
            .unwrap();
        db.put_notification("agent_done", "linear:A-2", "done", "/wt")
            .unwrap();
        assert_eq!(db.get_all_notifications(10).unwrap().len(), 2);
        db.delete_notification(id).unwrap();
        let remaining = db.get_all_notifications(10).unwrap();
        assert_eq!(remaining.len(), 1);
        assert_ne!(remaining[0].id, id);
    }

    #[test]
    fn get_unread_counts_by_worktree_groups_by_path() {
        let db = db();
        // Create notifications for different worktrees
        db.put_notification("assigned", "ref:1", "msg", "/wt/app")
            .unwrap();
        db.put_notification("mentioned", "ref:2", "msg", "/wt/app")
            .unwrap();
        db.put_notification("status_changed", "ref:3", "msg", "/wt/other")
            .unwrap();
        // Read one to make it not count as unread
        let unread = db.get_unread_notifications().unwrap();
        assert_eq!(unread.len(), 3);
        db.mark_notification_read(unread[0].id).unwrap();

        let counts = db.get_unread_counts_by_worktree().unwrap();
        // /wt/app has 1 unread, /wt/other has 1 unread
        assert_eq!(counts.get("/wt/app"), Some(&1));
        assert_eq!(counts.get("/wt/other"), Some(&1));
    }

    #[test]
    fn get_alert_counts_by_worktree_filters_by_kind() {
        let db = db();
        // Create various notification types
        db.put_notification("assigned", "ref:1", "msg", "/wt/app")
            .unwrap(); // not an alert
        db.put_notification("test_failed", "ref:2", "tests failed", "/wt/app")
            .unwrap();
        db.put_notification("agent_failed", "ref:3", "agent died", "/wt/app")
            .unwrap();
        db.put_notification("log_error", "ref:4", "error log", "/wt/other")
            .unwrap();
        db.put_notification("assigned", "ref:5", "msg", "/wt/other")
            .unwrap(); // not an alert

        let counts = db.get_alert_counts_by_worktree().unwrap();
        // /wt/app has 2 alerts (test_failed + agent_failed)
        // /wt/other has 1 alert (log_error)
        assert_eq!(counts.get("/wt/app"), Some(&2));
        assert_eq!(counts.get("/wt/other"), Some(&1));
    }

    #[test]
    fn process_failed_is_an_alert_process_exited_is_only_unread() {
        let db = db();
        // A clean task completion: unread, but NOT an alert.
        db.put_notification("process_exited", "make", "make finished", "/wt/app")
            .unwrap();
        // A failure: both unread and an alert (red badge).
        db.put_notification(
            "process_failed",
            "cargo",
            "cargo failed (exit 101)",
            "/wt/app",
        )
        .unwrap();

        let unread = db.get_unread_counts_by_worktree().unwrap();
        assert_eq!(unread.get("/wt/app"), Some(&2), "both count toward unread");

        let alerts = db.get_alert_counts_by_worktree().unwrap();
        assert_eq!(
            alerts.get("/wt/app"),
            Some(&1),
            "only process_failed is an alert"
        );
    }

    // ── Suite C: container_events audit trail ──────────────────────────────

    #[test]
    fn container_events_round_trip() {
        let db = db();
        db.insert_container_event("/wt/feat", 1000, "exec", Some("cargo build"), None)
            .unwrap();
        db.insert_container_event("/wt/feat", 2000, "exec", Some("git status"), Some(0))
            .unwrap();
        db.insert_container_event("/wt/other", 3000, "die", None, Some(1))
            .unwrap();

        let events = db.container_events("/wt/feat", 10).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].ts, 2000, "newest first");
        assert_eq!(events[1].kind, "exec");
        assert_eq!(events[1].detail.as_deref(), Some("cargo build"));

        let other = db.container_events("/wt/other", 10).unwrap();
        assert_eq!(other.len(), 1);
        assert_eq!(other[0].exit_code, Some(1));
    }

    #[test]
    fn container_events_prune_removes_old() {
        let db = db();
        let now = crate::util::now();
        db.insert_container_event("/wt/feat", now - 86400, "exec", Some("old"), None)
            .unwrap();
        db.insert_container_event("/wt/feat", now - 100, "exec", Some("recent"), None)
            .unwrap();
        db.insert_container_event("/wt/feat", now, "exec", Some("now"), None)
            .unwrap();
        db.prune_container_events(3600).unwrap();
        let remaining = db.container_events("/wt/feat", 10).unwrap();
        assert_eq!(remaining.len(), 2, "only the 24h-old row should be pruned");
        assert!(
            remaining.iter().all(|e| e.detail.as_deref() != Some("old")),
            "old event must not appear in results"
        );
    }

    #[test]
    fn container_events_limit_honoured() {
        let db = db();
        for i in 0..15i64 {
            db.insert_container_event("/wt/feat", i, "exec", None, None)
                .unwrap();
        }
        let ten = db.container_events("/wt/feat", 10).unwrap();
        assert_eq!(ten.len(), 10);
    }
}
