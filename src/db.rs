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

use crate::models::{WorkspaceRow, WorktreeRow};
use crate::util;
use anyhow::Result;
use rusqlite::{Connection, params};
use std::path::PathBuf;

/// Schema version. Bumped to 2 for the workspace=session / worktree=tab remap.
const SCHEMA_VERSION: i64 = 3;

pub struct Db {
    conn: Connection,
}

fn db_path() -> PathBuf {
    util::xdg_state_home().join("superzej/superzej.db")
}

/// The zellij session name (or "default" outside a session). In the v2 model a
/// session *is* a workspace, so this doubles as the current workspace key.
pub fn session() -> String {
    std::env::var("ZELLIJ_SESSION_NAME").unwrap_or_else(|_| "default".into())
}

impl Db {
    pub fn open() -> Result<Db> {
        let path = db_path();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let conn = Connection::open(&path)?;
        conn.busy_timeout(std::time::Duration::from_millis(5000))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;

        // Migrate. v2→v3 collapses the per-repo-session model into one session
        // where each repo/worktree is a tab, so `workspaces` is re-keyed by
        // repo_path (was session_name) and `worktrees.session_name` becomes the
        // single UI session. Neither has a faithful transform — drop and
        // recreate. The `repos` recents history is preserved (it's the only
        // irreplaceable data); git + live tabs re-discover everything else.
        let ver: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap_or(0);
        if ver < SCHEMA_VERSION {
            conn.execute_batch(
                "DROP TABLE IF EXISTS tabs;
                 DROP TABLE IF EXISTS worktrees;
                 DROP TABLE IF EXISTS workspaces;",
            )?;
            // Add the session_name column to a pre-existing repos table (no-op /
            // ignored error on a fresh DB, where the CREATE below adds it).
            let _ = conn.execute("ALTER TABLE repos ADD COLUMN session_name TEXT", []);
            conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        }

        conn.execute_batch(
            r#"
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
              created_at   INTEGER
            );
            CREATE TABLE IF NOT EXISTS pr_cache (
              worktree   TEXT PRIMARY KEY,
              branch     TEXT,
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
            "#,
        )?;
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

    // --- workspaces (a registered repo) ------------------------------------
    /// Record (or refresh) a registered repo. Keyed by repo path — all repos
    /// share the one UI session now.
    pub fn put_workspace(&self, repo_path: &str, name: &str) -> Result<()> {
        let now = util::now();
        self.conn.execute(
            r#"INSERT INTO workspaces(repo_path,name,created_at,last_active)
               VALUES(?1,?2,?3,?3)
               ON CONFLICT(repo_path) DO UPDATE SET name=?2, last_active=?3"#,
            params![repo_path, name, now],
        )?;
        Ok(())
    }

    /// A stable, globally-unique slug for a repo (the prefix of all its tabs).
    /// Reuses the previously-assigned slug; otherwise takes `base`, suffixing
    /// `-2`, `-3`, … on collision with a *different* repo, then persists it.
    /// Two repos with the same basename therefore get distinct tab namespaces.
    pub fn slug_for_repo(&self, repo_path: &str, base: &str) -> Result<String> {
        if let Ok(s) = self.conn.query_row(
            "SELECT slug FROM repo_slugs WHERE repo_path=?1",
            params![repo_path],
            |r| r.get::<_, String>(0),
        ) {
            if !s.is_empty() {
                return Ok(s);
            }
        }
        let taken: std::collections::HashSet<String> = {
            let mut stmt = self
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
        self.conn.execute(
            "INSERT OR REPLACE INTO repo_slugs(repo_path, slug) VALUES(?1, ?2)",
            params![repo_path, cand],
        )?;
        Ok(cand)
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
            "SELECT repo_path, name, created_at, last_active
             FROM workspaces ORDER BY last_active DESC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(WorkspaceRow {
                repo_path: r.get(0)?,
                name: r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                created_at: r.get::<_, Option<i64>>(2)?.unwrap_or(0),
                last_active: r.get::<_, Option<i64>>(3)?.unwrap_or(0),
            })
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    // --- worktrees (one per tab; keyed by worktree path) -------------------
    pub fn put_worktree(&self, tab: &str, root: &str, wt: &str, branch: &str) -> Result<()> {
        self.conn.execute(
            r#"INSERT INTO worktrees(worktree,session_name,tab_name,repo_path,branch,agent,created_at)
               VALUES(?1,?2,?3,?4,?5,'',?6)
               ON CONFLICT(worktree) DO UPDATE SET branch=?5, tab_name=?3, repo_path=?4, session_name=?2"#,
            params![wt, session(), tab, root, branch, util::now()],
        )?;
        Ok(())
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
        let mut stmt = self.conn.prepare(
            "SELECT worktree, branch, agent, created_at, repo_path, tab_name, session_name
             FROM worktrees",
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
            })
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }
}
