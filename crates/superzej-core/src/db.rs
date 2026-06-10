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
use rusqlite::{Connection, OptionalExtension, params};
use std::path::PathBuf;

/// Schema version. v3: workspace=session / worktree=tab remap. v4 (native host):
/// adds `tab_layout` + `session_state` for DB-driven session resurrect (the
/// native compositor owns layout, which zellij owned before) — purely additive.
const SCHEMA_VERSION: i64 = 4;

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
              created_at   INTEGER,
              location     TEXT
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
            CREATE TABLE IF NOT EXISTS loc_cache (
              worktree   TEXT PRIMARY KEY,
              loc        INTEGER,
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
            -- v4: the native host owns the layout zellij used to own. One row per
            -- tab: its kind, the worktree it belongs to (NULL for home/pinned),
            -- the serialized pane tree (CenterTree JSON), order, and which leaf
            -- had focus — enough to rebuild every tab on resurrect.
            CREATE TABLE IF NOT EXISTS tab_layout (
              session_name TEXT,
              tab_name     TEXT,
              kind         TEXT,
              worktree     TEXT,
              pane_tree    TEXT,
              ordinal      INTEGER,
              focused_pane INTEGER,
              PRIMARY KEY (session_name, tab_name)
            );
            -- v4: which tab was active at exit, per session.
            CREATE TABLE IF NOT EXISTS session_state (
              session_name TEXT PRIMARY KEY,
              active_tab   TEXT,
              updated_at   INTEGER
            );
            -- Switch/panel-resolve hot path: worktree lookup keyed by the tab.
            CREATE INDEX IF NOT EXISTS idx_worktrees_session_tab
              ON worktrees (session_name, tab_name);
            "#,
        )?;
        // Additive: a pre-existing v3 worktrees table predates the remote-worktree
        // `location` column. Add it in place (ignored if already present) so local
        // worktree history survives — no full migration/reset needed.
        let _ = conn.execute("ALTER TABLE worktrees ADD COLUMN location TEXT", []);
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

    // --- LOC cache ---------------------------------------------------------
    pub fn get_loc_cache(&self, worktree: &str) -> Result<Option<usize>> {
        let r = self
            .conn
            .query_row(
                "SELECT loc FROM loc_cache WHERE worktree=?1",
                params![worktree],
                |r| r.get::<_, usize>(0),
            )
            .ok();
        Ok(r)
    }

    pub fn put_loc_cache(&self, worktree: &str, loc: usize) -> Result<()> {
        self.conn.execute(
            r#"INSERT INTO loc_cache(worktree,loc,fetched_at)
               VALUES(?1,?2,?3)
               ON CONFLICT(worktree) DO UPDATE SET loc=?2, fetched_at=?3"#,
            params![worktree, loc, util::now()],
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
        self.conn.execute(
            r#"INSERT INTO worktrees(worktree,session_name,tab_name,repo_path,branch,agent,created_at,location)
               VALUES(?1,?2,?3,?4,?5,'',?6,?7)
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
            "SELECT worktree, branch, agent, created_at, repo_path, tab_name, session_name, location
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
                location: r.get::<_, Option<String>>(7)?.unwrap_or_default(),
            })
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    // --- v4 session/layout persistence (native-host resurrect) -------------

    /// Insert or replace a tab's persisted layout.
    pub fn put_tab_layout(&self, session: &str, row: &crate::models::TabLayoutRow) -> Result<()> {
        self.conn.execute(
            "INSERT INTO tab_layout
               (session_name, tab_name, kind, worktree, pane_tree, ordinal, focused_pane)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(session_name, tab_name) DO UPDATE SET
               kind=?3, worktree=?4, pane_tree=?5, ordinal=?6, focused_pane=?7",
            params![
                session,
                row.tab_name,
                row.kind,
                row.worktree,
                row.pane_tree,
                row.ordinal,
                row.focused_pane,
            ],
        )?;
        Ok(())
    }

    /// All persisted tabs for a session, in display order (for resurrect).
    pub fn tabs_for_session(&self, session: &str) -> Result<Vec<crate::models::TabLayoutRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT tab_name, kind, worktree, pane_tree, ordinal, focused_pane
               FROM tab_layout WHERE session_name=?1 ORDER BY ordinal",
        )?;
        let rows = stmt.query_map(params![session], |r| {
            Ok(crate::models::TabLayoutRow {
                tab_name: r.get(0)?,
                kind: r.get(1)?,
                worktree: r.get::<_, Option<String>>(2)?.unwrap_or_default(),
                pane_tree: r.get::<_, Option<String>>(3)?.unwrap_or_default(),
                ordinal: r.get::<_, Option<i64>>(4)?.unwrap_or(0),
                focused_pane: r.get::<_, Option<i64>>(5)?.unwrap_or(0),
            })
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// Forget a tab's persisted layout (on close).
    pub fn delete_tab_layout(&self, session: &str, tab: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM tab_layout WHERE session_name=?1 AND tab_name=?2",
            params![session, tab],
        )?;
        Ok(())
    }

    /// Record which tab is active (for restoring focus on resurrect).
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Db {
        Db::open_memory().unwrap()
    }

    #[test]
    fn tab_layout_roundtrip_ordered_by_ordinal() {
        use crate::models::TabLayoutRow;
        let db = db();
        let sess = "s1";
        let mk = |name: &str, ord: i64| TabLayoutRow {
            tab_name: name.into(),
            kind: "worktree".into(),
            worktree: format!("/wt/{name}"),
            pane_tree: r#"{"leaf":0}"#.into(),
            ordinal: ord,
            focused_pane: 0,
        };
        // Insert out of order; expect ordinal ordering back.
        db.put_tab_layout(sess, &mk("app/feat", 1)).unwrap();
        db.put_tab_layout(sess, &mk("app/home", 0)).unwrap();
        let rows = db.tabs_for_session(sess).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].tab_name, "app/home");
        assert_eq!(rows[1].tab_name, "app/feat");

        // Upsert replaces in place (no duplicate row).
        db.put_tab_layout(sess, &mk("app/feat", 5)).unwrap();
        let rows = db.tabs_for_session(sess).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows.iter()
                .find(|r| r.tab_name == "app/feat")
                .unwrap()
                .ordinal,
            5
        );

        // Delete removes just that tab; other session is untouched.
        db.put_tab_layout("other", &mk("x/home", 0)).unwrap();
        db.delete_tab_layout(sess, "app/feat").unwrap();
        assert_eq!(db.tabs_for_session(sess).unwrap().len(), 1);
        assert_eq!(db.tabs_for_session("other").unwrap().len(), 1);
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
        db.put_workspace("/repo", "repo").unwrap();
        db.put_workspace("/repo", "repo2").unwrap(); // upsert renames
        let ws = db.workspaces().unwrap();
        assert_eq!(ws.len(), 1);
        assert_eq!(ws[0].repo_path, "/repo");
        assert_eq!(ws[0].name, "repo2");
        assert!(db.is_known_repo("/repo").unwrap());
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
    }

    #[test]
    fn worktree_crud() {
        let db = db();
        db.put_worktree("app/feat", "/x/app", "/wt/feat", "sz/feat", None)
            .unwrap();
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
            "/wt/feat",
            "sz/feat",
            Some("{\"host\":\"box\"}"),
        )
        .unwrap();
        assert_eq!(
            db.location_for("/wt/feat").unwrap().as_deref(),
            Some("{\"host\":\"box\"}")
        );
        // delete
        db.del_worktree("/wt/feat").unwrap();
        assert!(db.worktrees().unwrap().is_empty());
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
}
