//! SQLite-backed state & history (replaces the old JSON files).
//!
//! One global DB at `$XDG_STATE_HOME/superzej/superzej.db` with three tables:
//!   repos      — every repo ever opened (the launcher's "recents")
//!   tabs       — workspace tab -> repo, per session
//!   worktrees  — superzej-managed worktrees (keyed by worktree path)
//!
//! git remains the source of truth for worktrees on disk; this is a cache +
//! history layer. rusqlite is bundled, so there's no system sqlite dependency.

use crate::models::WorktreeRow;
use crate::util;
use anyhow::Result;
use rusqlite::{Connection, params};
use std::path::PathBuf;

pub struct Db {
    conn: Connection,
}

fn db_path() -> PathBuf {
    util::xdg_state_home().join("superzej/superzej.db")
}

/// The zellij session name (or "default" outside a session).
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
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS repos (
              path        TEXT PRIMARY KEY,
              name        TEXT,
              first_seen  INTEGER,
              last_opened INTEGER,
              open_count  INTEGER DEFAULT 0,
              seq         INTEGER DEFAULT 0
            );
            CREATE TABLE IF NOT EXISTS tabs (
              session    TEXT,
              tab_name   TEXT,
              repo_path  TEXT,
              created_at INTEGER,
              PRIMARY KEY (session, tab_name)
            );
            CREATE TABLE IF NOT EXISTS worktrees (
              worktree   TEXT PRIMARY KEY,
              session    TEXT,
              tab_name   TEXT,
              repo_path  TEXT,
              branch     TEXT,
              agent      TEXT,
              created_at INTEGER
            );
            "#,
        )?;
        Ok(Db { conn })
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
             UNION SELECT repo_path FROM tabs
             UNION SELECT path FROM repos",
        )?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        Ok(rows
            .filter_map(|r| r.ok())
            .filter(|s| !s.is_empty())
            .collect())
    }

    // --- tabs --------------------------------------------------------------
    pub fn put_tab(&self, name: &str, root: &str) -> Result<()> {
        self.conn.execute(
            r#"INSERT INTO tabs(session,tab_name,repo_path,created_at)
               VALUES(?1,?2,?3,?4)
               ON CONFLICT(session,tab_name) DO UPDATE SET repo_path=?3"#,
            params![session(), name, root, util::now()],
        )?;
        Ok(())
    }

    #[allow(dead_code)]
    pub fn tab_root(&self, name: &str) -> Result<Option<String>> {
        let r = self
            .conn
            .query_row(
                "SELECT repo_path FROM tabs WHERE session=?1 AND tab_name=?2 LIMIT 1",
                params![session(), name],
                |r| r.get::<_, String>(0),
            )
            .ok();
        Ok(r)
    }

    // --- worktrees (keyed by worktree path) --------------------------------
    pub fn put_worktree(&self, tab: &str, root: &str, wt: &str, branch: &str) -> Result<()> {
        self.conn.execute(
            r#"INSERT INTO worktrees(worktree,session,tab_name,repo_path,branch,agent,created_at)
               VALUES(?1,?2,?3,?4,?5,'',?6)
               ON CONFLICT(worktree) DO UPDATE SET branch=?5, tab_name=?3, repo_path=?4"#,
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

    /// All recorded worktrees (metadata only; git supplies live status).
    pub fn worktrees(&self) -> Result<Vec<WorktreeRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT worktree, branch, agent, created_at, repo_path, tab_name FROM worktrees",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(WorktreeRow {
                worktree: r.get(0)?,
                branch: r.get(1)?,
                agent: r.get(2)?,
                created_at: r.get(3)?,
                repo_root: r.get(4)?,
                tab_name: r.get(5)?,
            })
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }
}
