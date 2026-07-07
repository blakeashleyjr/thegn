//! Legacy one-shot schema migrations extracted from `db.rs` (pinned by the
//! file-size ratchet). These run inside [`crate::db::Db`]'s `init()` ladder and
//! are exercised by the ladder tests in `db.rs`.

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};

impl crate::db::Db {
    /// The on-disk schema version when it is newer than this build understands,
    /// else `None`. Data the newer build wrote under tables/columns this build
    /// doesn't know about may be invisible; the host surfaces a warning once.
    pub fn schema_mismatch(&self) -> Option<i64> {
        self.schema_mismatch
    }
}

/// Classify the on-disk `user_version` against `current` (this build's
/// [`crate::db::SCHEMA_VERSION`]): `Some(on_disk)` when the DB was written by a
/// newer-schema build (a different branch sharing the file), else `None`. We
/// warn and keep opening — the multi-branch-one-DB dev workflow is intentional
/// and the additive schema is forward-compatible — but record it so the host
/// can surface the mismatch once at startup.
pub(crate) fn detect_newer_schema(on_disk: i64, current: i64) -> Option<i64> {
    let newer = (on_disk > current).then_some(on_disk)?;
    tracing::warn!(
        target: "szhost::db",
        on_disk = newer,
        build = current,
        "database schema v{newer} is newer than this build (v{current}); \
         data written by the newer build may be invisible"
    );
    Some(newer)
}

/// Split a legacy v4/v5 tab name into its worktree-group base and page number:
/// `"app/feat ·3"` → `("app/feat", Some(3))`, `"app/feat"` → `("app/feat", None, None)`.
pub(crate) fn split_page_suffix(name: &str) -> (&str, Option<u32>) {
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
pub(crate) fn migrate_tab_layout_v6(conn: &Connection) {
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

/// The additive schema-evolution ladder: columns and tables bolted onto a
/// pre-existing DB in place (every statement idempotent / ignored when
/// already applied), so upgrades never reset user data. Called from
/// `Db::init` after the base CREATEs.
pub(crate) fn additive_schema(conn: &Connection) {
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
    // v14: per-leaf working directories (JSON map of pane id → cwd) so
    // resurrected panes respawn where they last were, not at the worktree
    // root. Additive; absent/NULL on pre-v14 rows = no cwd hints.
    let _ = conn.execute("ALTER TABLE group_tabs ADD COLUMN pane_cwds TEXT", []);
    // v15: per-leaf last foreground command (JSON map of pane id →
    // {argv, cwd}) so a resurrected/crashed pane can offer to relaunch the
    // program it was running. Additive; absent/NULL on pre-v15 rows = none.
    let _ = conn.execute("ALTER TABLE group_tabs ADD COLUMN pane_cmds TEXT", []);
    // v23: per-leaf provider exec session (JSON map of pane id →
    // {provider, id, session}) so a native-exec pane reattaches to its live
    // remote session on restart. Additive; absent/NULL on pre-v23 rows = none.
    let _ = conn.execute("ALTER TABLE group_tabs ADD COLUMN pane_sessions TEXT", []);
    // v26: warm spare-sandbox pool. `pool_spares` tracks pre-provisioned,
    // UNCLAIMED sandboxes per (repo, env) so a new worktree opens instantly by
    // claiming one; `pool_targets` is the runtime +/- override of the configured
    // `[lifecycle.pool]` size; `worktrees.provider_sandbox_id` binds a worktree
    // to the spare it claimed (overrides the derived sandbox name).
    let _ = conn.execute(
        "CREATE TABLE IF NOT EXISTS pool_spares (
           sandbox_name  TEXT PRIMARY KEY,
           repo_path     TEXT NOT NULL,
           env_name      TEXT NOT NULL,
           state         TEXT NOT NULL,
           checkpoint_id TEXT,
           lock_hash     TEXT,
           created_at    INTEGER NOT NULL,
           updated_at    INTEGER NOT NULL
         )",
        [],
    );
    let _ = conn.execute(
        "CREATE TABLE IF NOT EXISTS pool_targets (
           repo_path TEXT NOT NULL,
           env_name  TEXT NOT NULL,
           target    INTEGER NOT NULL,
           PRIMARY KEY (repo_path, env_name)
         )",
        [],
    );
    // v38: worktrees whose provider compute was (or is being) snapshot-then-
    // destroyed. Intent-ordered like the VPS ledger: 'capturing' BEFORE the
    // capture starts, 'hibernated' only after the snapshot verified into the
    // [lifecycle.snapshot] store (then destroy), 'restoring' while a re-open
    // replays it. A 'hibernated' row + a live instance means a crash
    // interrupted the destroy — the hibernator re-verifies and finishes.
    let _ = conn.execute(
        "CREATE TABLE IF NOT EXISTS worktree_hibernations (
           worktree_path TEXT PRIMARY KEY,
           repo_path     TEXT NOT NULL,
           env_name      TEXT NOT NULL,
           sandbox_name  TEXT NOT NULL,
           snapshot_id   TEXT NOT NULL,
           head          TEXT,
           state         TEXT NOT NULL,
           created_at    INTEGER NOT NULL,
           updated_at    INTEGER NOT NULL
         )",
        [],
    );
    let _ = conn.execute(
        "ALTER TABLE worktrees ADD COLUMN provider_sandbox_id TEXT",
        [],
    );
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
    // v16: a persistent per-workspace sort key — the source of truth for
    // sidebar workspace order (was `last_active DESC`). Additive; backfilled
    // below.
    let _ = conn.execute("ALTER TABLE workspaces ADD COLUMN position INTEGER", []);
    // Backfill from the prior recency order: position 0 = most-recently
    // active (recency is DESC, hence `>` here vs the worktrees' `<`), with
    // repo_path as the collision-free tie-breaker. Runs once: after this
    // every row has a position, and `put_workspace` assigns MAX+1.
    let _ = conn.execute(
        "UPDATE workspaces SET position = (
             SELECT COUNT(*) FROM workspaces AS w2
             WHERE (w2.last_active, w2.repo_path) > (workspaces.last_active, workspaces.repo_path)
         ) WHERE position IS NULL",
        [],
    );

    // v17: folders table and worktrees.folder_id
    let _ = conn.execute(
        "CREATE TABLE IF NOT EXISTS folders (
            folder_id INTEGER PRIMARY KEY,
            repo_path TEXT NOT NULL REFERENCES workspaces(repo_path) ON DELETE CASCADE,
            name TEXT NOT NULL,
            position INTEGER NOT NULL,
            created_at INTEGER NOT NULL
         )",
        [],
    );
    let _ = conn.execute(
        "CREATE TABLE IF NOT EXISTS terminals (
          id                INTEGER PRIMARY KEY AUTOINCREMENT,
          name              TEXT    NOT NULL UNIQUE,
          kind              TEXT    NOT NULL,
          connection_string TEXT    NOT NULL,
          folder_id         INTEGER,
          created_at        INTEGER NOT NULL,
          last_active       INTEGER NOT NULL,
          position          INTEGER NOT NULL DEFAULT 0,
          sandbox_backend   TEXT,
          env_name          TEXT
        )",
        [],
    );
    // Per-terminal sandbox + env for DBs created before these columns existed
    // (additive, branch-merge-safe — no version bump; the ALTER is a no-op once
    // the column exists). A local terminal can launch wrapped in a sandbox /
    // named env just like a worktree pane.
    let _ = conn.execute("ALTER TABLE terminals ADD COLUMN sandbox_backend TEXT", []);
    let _ = conn.execute("ALTER TABLE terminals ADD COLUMN env_name TEXT", []);
    let _ = conn.execute("ALTER TABLE worktrees ADD COLUMN folder_id INTEGER", []);
    // v21: per-worktree ingress shares (`[share]`). A worktree can expose
    // several ports, so the key is (worktree, local_port). Additive; a row
    // is the resurrection record for a tunnel the host respawns on restart.
    let _ = conn.execute(
        "CREATE TABLE IF NOT EXISTS shares (
          worktree   TEXT    NOT NULL,
          local_port INTEGER NOT NULL,
          provider   TEXT    NOT NULL,
          public_url TEXT,
          state      TEXT    NOT NULL,
          created_at INTEGER NOT NULL,
          PRIMARY KEY (worktree, local_port)
        )",
        [],
    );
    // v23: auto port forwards (`[forward]`). A worktree can forward several
    // ports, so the key is (worktree, container_port). Additive; a row is the
    // resurrection record so the host re-detects forwards on restart.
    let _ = conn.execute(
        "CREATE TABLE IF NOT EXISTS forwards (
          worktree       TEXT    NOT NULL,
          container_port INTEGER NOT NULL,
          host_port      INTEGER NOT NULL,
          url            TEXT    NOT NULL,
          created_at     INTEGER NOT NULL,
          PRIMARY KEY (worktree, container_port)
        )",
        [],
    );
    // v18: the named execution environment selected per workspace/worktree
    // (`[env.<name>]`). Additive; absent/NULL = inherit the next layer down
    // (worktree → workspace → repo `.superzej.*` → global default → default).
    let _ = conn.execute("ALTER TABLE workspaces ADD COLUMN env_name TEXT", []);
    let _ = conn.execute("ALTER TABLE worktrees ADD COLUMN env_name TEXT", []);
    // v27: persisted vim-style registers (Phase 3 of time-travel-replay).
    // Additive; keyed by the single-char register id. The `"+` clipboard
    // register is volatile and never written here.
    let _ = conn.execute(
        "CREATE TABLE IF NOT EXISTS registers (
          name       TEXT PRIMARY KEY,
          value      BLOB NOT NULL,
          updated_at INTEGER NOT NULL
        )",
        [],
    );
    // v29: per-leaf captured scrollback tail (JSON map of pane id → text) so a
    // resurrected pane repaints its recent history. Additive; NULL pre-v29.
    let _ = conn.execute(
        "ALTER TABLE group_tabs ADD COLUMN scrollback_snapshot TEXT",
        [],
    );
    // v32: trust-on-first-use approvals for a repo `.superzej.*` overlay's
    // gated sandbox requests (mounts/scripts/image/…). One row per approved
    // request, keyed by (repo_root, canonical request JSON) — the canonical
    // string is the security match key, so a later edit to the requested set
    // re-prompts. `request_id` is a short display handle only. See
    // `crate::config_resolve` / `crate::repo_trust`.
    let _ = conn.execute(
        "CREATE TABLE IF NOT EXISTS repo_trust (
          repo_root    TEXT NOT NULL,
          request_id   TEXT NOT NULL,
          request_json TEXT NOT NULL,
          decision     TEXT NOT NULL,
          decided_at   INTEGER NOT NULL,
          PRIMARY KEY (repo_root, request_json)
        )",
        [],
    );
    // v33: zones — a named group of workspaces inside a profile providing a
    // soft, concurrent firewall (credential sub-vault + egress/budget ceilings).
    // Membership is a nullable `workspaces.zone_id` (NULL = unzoned); exclusive
    // by construction (one column, not a join table). Policy lives in config
    // (`[zone.<name>]`); the DB owns existence + membership. See `crate::zone`.
    let _ = conn.execute(
        "CREATE TABLE IF NOT EXISTS zones (
          zone_id    INTEGER PRIMARY KEY,
          name       TEXT NOT NULL UNIQUE,
          created_at INTEGER NOT NULL
        )",
        [],
    );
    let _ = conn.execute("ALTER TABLE workspaces ADD COLUMN zone_id INTEGER", []);
}

#[cfg(test)]
mod tests {
    use crate::db::Db;
    use crate::store::WorkspaceStore;

    #[test]
    fn detect_newer_schema_flags_only_a_newer_db() {
        // Older / equal on-disk versions are fine; only a strictly-newer DB
        // (written by a different-schema branch build) is flagged.
        assert_eq!(super::detect_newer_schema(5, 10), None);
        assert_eq!(super::detect_newer_schema(10, 10), None);
        assert_eq!(super::detect_newer_schema(12, 10), Some(12));
    }

    #[test]
    fn a_fresh_db_reports_no_schema_mismatch() {
        // A DB this build just created is at its own version — never a mismatch.
        assert_eq!(Db::open_memory().unwrap().schema_mismatch(), None);
    }

    #[test]
    fn terminal_row_roundtrips_sandbox_and_env() {
        let db = Db::open_memory().unwrap();
        db.put_terminal("local", "local", "", None).unwrap();
        db.set_terminal_sandbox("local", "bwrap").unwrap();
        db.set_terminal_env("local", "dev").unwrap();
        let t = db
            .terminals()
            .unwrap()
            .into_iter()
            .find(|t| t.name == "local")
            .unwrap();
        assert_eq!(t.sandbox_backend, "bwrap");
        assert_eq!(t.env_name, "dev");

        // A fresh terminal has empty sandbox/env (COALESCE default), so the
        // sidebar/chip render as an uncontained local shell.
        db.put_terminal("plain", "local", "", None).unwrap();
        let p = db
            .terminals()
            .unwrap()
            .into_iter()
            .find(|t| t.name == "plain")
            .unwrap();
        assert_eq!(p.sandbox_backend, "");
        assert_eq!(p.env_name, "");
    }
}
