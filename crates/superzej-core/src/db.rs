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

/// Schema version. v3: workspace / worktree remap. v4 (native host): adds
/// `tab_layout` + `session_state` for DB-driven session resurrect (the native
/// compositor owns layout). v5: adds the `ui_state` key-value table backing the
/// sidebar's persisted view state (collapse, sort mode, bar width, pin order) —
/// purely additive. v6: tabs live *within* a worktree — the flat `tab_layout`
/// (pages encoded as " ·N" name suffixes) becomes `tab_groups` + `group_tabs`;
/// legacy rows are transformed in place and `tab_layout` is dropped.
const SCHEMA_VERSION: i64 = 6;

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

    /// As [`Db::get_loc_cache`], with the fetch timestamp (for TTL refresh).
    pub fn get_loc_cache_entry(&self, worktree: &str) -> Result<Option<(usize, i64)>> {
        let r = self
            .conn
            .query_row(
                "SELECT loc, fetched_at FROM loc_cache WHERE worktree=?1",
                params![worktree],
                |r| Ok((r.get::<_, usize>(0)?, r.get::<_, i64>(1)?)),
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
}
