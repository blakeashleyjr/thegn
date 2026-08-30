//! Legacy one-shot schema migrations extracted from `db.rs` (pinned by the
//! keep-god-files-flat guidance). These run inside [`crate::db::Db`]'s `init()` ladder and
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
/// newer-schema build (a different branch sharing the file), else `None`.
/// Pure classifier — the caller (e.g. [`crate::db::Db::init`]) is responsible
/// for emitting the once-per-process warning.
pub(crate) fn detect_newer_schema(on_disk: i64, current: i64) -> Option<i64> {
    (on_disk > current).then_some(on_disk)
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
        tracing::warn!(target: "thegn::db", error = %e, "v6 tab_layout migration failed; keeping legacy table");
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
    // best-effort: idempotent additive migration: the ignore is the already-applied no-op
    let _ = conn.execute("ALTER TABLE worktrees ADD COLUMN location TEXT", []);
    // Additive: running-pin set per session (JSON), so the native host can
    // resurrect strip/float pins (the pin supervisor re-launches them).
    // best-effort: idempotent additive migration: the ignore is the already-applied no-op
    let _ = conn.execute("ALTER TABLE session_state ADD COLUMN pin_state TEXT", []);
    // Additive: a workspace's kind — "repo" (a git repo) or "dir" (a plain
    // non-git directory). Defaults keep every pre-existing workspace a repo.
    // best-effort: idempotent additive migration: the ignore is the already-applied no-op
    let _ = conn.execute(
        "ALTER TABLE workspaces ADD COLUMN kind TEXT DEFAULT 'repo'",
        [],
    );
    // v8: a persistent per-worktree sort key — the single source of truth
    // for sidebar order (loaded + unloaded). Additive; backfilled below.
    // best-effort: idempotent additive migration: the ignore is the already-applied no-op
    let _ = conn.execute("ALTER TABLE worktrees ADD COLUMN position INTEGER", []);
    // v14: per-leaf working directories (JSON map of pane id → cwd) so
    // resurrected panes respawn where they last were, not at the worktree
    // root. Additive; absent/NULL on pre-v14 rows = no cwd hints.
    // best-effort: idempotent additive migration: the ignore is the already-applied no-op
    let _ = conn.execute("ALTER TABLE group_tabs ADD COLUMN pane_cwds TEXT", []);
    // v15: per-leaf last foreground command (JSON map of pane id →
    // {argv, cwd}) so a resurrected/crashed pane can offer to relaunch the
    // program it was running. Additive; absent/NULL on pre-v15 rows = none.
    // best-effort: idempotent additive migration: the ignore is the already-applied no-op
    let _ = conn.execute("ALTER TABLE group_tabs ADD COLUMN pane_cmds TEXT", []);
    // v23: per-leaf provider exec session (JSON map of pane id →
    // {provider, id, session}) so a native-exec pane reattaches to its live
    // remote session on restart. Additive; absent/NULL on pre-v23 rows = none.
    // best-effort: idempotent additive migration: the ignore is the already-applied no-op
    let _ = conn.execute("ALTER TABLE group_tabs ADD COLUMN pane_sessions TEXT", []);
    // v26: warm spare-sandbox pool. `pool_spares` tracks pre-provisioned,
    // UNCLAIMED sandboxes per (repo, env) so a new worktree opens instantly by
    // claiming one; `pool_targets` is the runtime +/- override of the configured
    // `[lifecycle.pool]` size; `worktrees.provider_sandbox_id` binds a worktree
    // to the spare it claimed (overrides the derived sandbox name).
    // best-effort: idempotent additive migration: the ignore is the already-applied no-op
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
    // best-effort: idempotent additive migration: the ignore is the already-applied no-op
    let _ = conn.execute(
        "CREATE TABLE IF NOT EXISTS pool_targets (
           repo_path TEXT NOT NULL,
           env_name  TEXT NOT NULL,
           target    INTEGER NOT NULL,
           PRIMARY KEY (repo_path, env_name)
         )",
        [],
    );
    // v39: worktrees whose provider compute was (or is being) snapshot-then-
    // destroyed. Intent-ordered like the VPS ledger: 'capturing' BEFORE the
    // capture starts, 'hibernated' only after the snapshot verified into the
    // [lifecycle.snapshot] store (then destroy), 'restoring' while a re-open
    // replays it. A 'hibernated' row + a live instance means a crash
    // interrupted the destroy — the hibernator re-verifies and finishes.
    // best-effort: idempotent additive migration: the ignore is the already-applied no-op
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
    // best-effort: idempotent additive migration: the ignore is the already-applied no-op
    let _ = conn.execute(
        "ALTER TABLE worktrees ADD COLUMN provider_sandbox_id TEXT",
        [],
    );
    // Backfill any unset positions deterministically by creation order
    // (path as the tie-breaker), giving pre-v8 worktrees a stable,
    // collision-free order on first launch after upgrade. Runs once: after
    // this every row has a position, and `put_worktree` assigns MAX+1.
    // best-effort: idempotent additive migration: the ignore is the already-applied no-op
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
    // best-effort: idempotent additive migration: the ignore is the already-applied no-op
    let _ = conn.execute("ALTER TABLE workspaces ADD COLUMN position INTEGER", []);
    // Backfill from the prior recency order: position 0 = most-recently
    // active (recency is DESC, hence `>` here vs the worktrees' `<`), with
    // repo_path as the collision-free tie-breaker. Runs once: after this
    // every row has a position, and `put_workspace` assigns MAX+1.
    // best-effort: idempotent additive migration: the ignore is the already-applied no-op
    let _ = conn.execute(
        "UPDATE workspaces SET position = (
             SELECT COUNT(*) FROM workspaces AS w2
             WHERE (w2.last_active, w2.repo_path) > (workspaces.last_active, workspaces.repo_path)
         ) WHERE position IS NULL",
        [],
    );

    // v17: folders table and worktrees.folder_id
    // best-effort: idempotent additive migration: the ignore is the already-applied no-op
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
    // best-effort: idempotent additive migration: the ignore is the already-applied no-op
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
    // best-effort: idempotent additive migration: the ignore is the already-applied no-op
    let _ = conn.execute("ALTER TABLE terminals ADD COLUMN sandbox_backend TEXT", []);
    // best-effort: idempotent additive migration: the ignore is the already-applied no-op
    let _ = conn.execute("ALTER TABLE terminals ADD COLUMN env_name TEXT", []);
    // best-effort: idempotent additive migration: the ignore is the already-applied no-op
    let _ = conn.execute("ALTER TABLE worktrees ADD COLUMN folder_id INTEGER", []);
    // Observed containment — what the last launch ACTUALLY entered, as derived
    // from its argv (`sandbox_truth`). Separate from `sandbox_backend`, which is
    // the user's PICK and stays a deliberate-override store driving
    // re-resolution: writing the observed value there would make the chip honest
    // and then lose the pick, so a user who later started their runtime would
    // silently get host shells forever. Display reads THIS column; resolution
    // reads the other. NULL = never launched, which displays as nothing rather
    // than as a guess. Additive, no version bump (same contract as above).
    // best-effort: idempotent additive migration: the ignore is the already-applied no-op
    let _ = conn.execute("ALTER TABLE terminals ADD COLUMN observed_backend TEXT", []);
    // best-effort: idempotent additive migration: the ignore is the already-applied no-op
    let _ = conn.execute("ALTER TABLE worktrees ADD COLUMN observed_backend TEXT", []);
    // v21: per-worktree ingress shares (`[share]`). A worktree can expose
    // several ports, so the key is (worktree, local_port). Additive; a row
    // is the resurrection record for a tunnel the host respawns on restart.
    // best-effort: idempotent additive migration: the ignore is the already-applied no-op
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
    // best-effort: idempotent additive migration: the ignore is the already-applied no-op
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
    // (worktree → workspace → repo `.thegn.*` → global default → default).
    // best-effort: idempotent additive migration: the ignore is the already-applied no-op
    let _ = conn.execute("ALTER TABLE workspaces ADD COLUMN env_name TEXT", []);
    // best-effort: idempotent additive migration: the ignore is the already-applied no-op
    let _ = conn.execute("ALTER TABLE worktrees ADD COLUMN env_name TEXT", []);
    // v27: persisted vim-style registers (Phase 3 of time-travel-replay).
    // Additive; keyed by the single-char register id. The `"+` clipboard
    // register is volatile and never written here.
    // best-effort: idempotent additive migration: the ignore is the already-applied no-op
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
    // best-effort: idempotent additive migration: the ignore is the already-applied no-op
    let _ = conn.execute(
        "ALTER TABLE group_tabs ADD COLUMN scrollback_snapshot TEXT",
        [],
    );
    // v32: trust-on-first-use approvals for a repo `.thegn.*` overlay's
    // gated sandbox requests (mounts/scripts/image/…). One row per approved
    // request, keyed by (repo_root, canonical request JSON) — the canonical
    // string is the security match key, so a later edit to the requested set
    // re-prompts. `request_id` is a short display handle only. See
    // `crate::config_resolve` / `crate::repo_trust`.
    // best-effort: idempotent additive migration: the ignore is the already-applied no-op
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
    // best-effort: idempotent additive migration: the ignore is the already-applied no-op
    let _ = conn.execute(
        "CREATE TABLE IF NOT EXISTS zones (
          zone_id    INTEGER PRIMARY KEY,
          name       TEXT NOT NULL UNIQUE,
          created_at INTEGER NOT NULL
        )",
        [],
    );
    // best-effort: idempotent additive migration: the ignore is the already-applied no-op
    let _ = conn.execute("ALTER TABLE workspaces ADD COLUMN zone_id INTEGER", []);
    // v44: the queued worktree's location (mirrored from `worktrees.location`)
    // so a cross-host merge-queue drain can attribute a row to a host and decide
    // whether the branch tip must be fetched into the target store. Additive;
    // NULL on pre-v44 rows = treated as local (same store as the target).
    // best-effort: idempotent additive migration: the ignore is the already-applied no-op
    let _ = conn.execute("ALTER TABLE merge_queue ADD COLUMN location TEXT", []);
    // v49: persist the agent-dispatch budget spent on a queue row. Additive;
    // pre-v49 rows start at 0, which is the pre-change behavior for their first
    // drain and correct thereafter.
    // best-effort: idempotent additive migration: the ignore is the already-applied no-op
    let _ = conn.execute(
        "ALTER TABLE merge_queue ADD COLUMN agent_attempts INTEGER NOT NULL DEFAULT 0",
        [],
    );
    // v50: `attention_acks` is re-keyed (worktree_path) → (worktree_path, reason)
    // and gains `episode`. SQLite cannot ALTER a primary key, and the base
    // `CREATE TABLE IF NOT EXISTS` is a no-op against the old-shaped table, so
    // this is a rebuild-and-copy. The rows are *copied*, not dropped: an ack the
    // user already made must not re-nag once just because the app upgraded.
    // Pre-v50 rows carry `episode = 0`, which still matches their (reason, since)
    // exactly — cache-derived signals gain a real episode on their next ack.
    if !has_column(conn, "attention_acks", "episode") {
        // best-effort: idempotent additive migration: the ignore is the already-applied no-op
        let _ = conn.execute_batch(
            "BEGIN;
             CREATE TABLE IF NOT EXISTS attention_acks_v50 (
               worktree_path TEXT    NOT NULL,
               reason        TEXT    NOT NULL,
               since         INTEGER,
               episode       INTEGER NOT NULL DEFAULT 0,
               acked_at      INTEGER NOT NULL DEFAULT 0,
               PRIMARY KEY (worktree_path, reason)
             );
             INSERT OR IGNORE INTO attention_acks_v50
               (worktree_path, reason, since, episode, acked_at)
               SELECT worktree_path, reason, since, 0, COALESCE(acked_at, 0)
               FROM attention_acks;
             DROP TABLE attention_acks;
             ALTER TABLE attention_acks_v50 RENAME TO attention_acks;
             COMMIT;",
        );
    }
    // v51: the PR queue. Purely additive — a `CREATE TABLE IF NOT EXISTS` here
    // (rather than an ALTER) so an older DB gains the table on open with every
    // other cache intact. The DDL is duplicated from `db.rs`'s init batch on
    // purpose: init creates it for a fresh DB, this creates it for an upgrade,
    // and both are idempotent.
    // best-effort: idempotent additive migration: the ignore is the already-applied no-op
    let _ = conn.execute(
        "CREATE TABLE IF NOT EXISTS pr_queue (
           key            TEXT PRIMARY KEY,
           repo_root      TEXT NOT NULL,
           number         INTEGER NOT NULL,
           worktree       TEXT,
           branch         TEXT NOT NULL,
           base_branch    TEXT NOT NULL DEFAULT '',
           forge          TEXT NOT NULL DEFAULT 'github',
           status         TEXT NOT NULL DEFAULT 'watching',
           blocker        TEXT,
           detail         TEXT,
           agent_attempts INTEGER NOT NULL DEFAULT 0,
           last_head_oid  TEXT,
           queued_at      INTEGER NOT NULL,
           updated_at     INTEGER NOT NULL
         )",
        [],
    );
    // best-effort: idempotent additive migration: the ignore is the already-applied no-op
    let _ = conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_pr_queue_repo ON pr_queue (repo_root, status, queued_at)",
        [],
    );
    // v54: projects — a grouping layer ABOVE workspaces (the zones *shape*, with
    // workflow semantics instead of policy: batched cross-repo worktree creation,
    // grouped navigation). Membership is a nullable `workspaces.project_id`
    // (NULL = unprojected); exclusive by construction (one column, not a join
    // table). `position` drives manual sidebar ordering of project headers, same
    // exact-order persistence as `set_workspace_order`. Projects carry ZERO
    // policy — assigning one never re-scopes credentials/egress/budget/sandbox
    // (that is zones' exclusive job); a project MAY span zones. No cross-repo
    // feature link rows are stored: git stays the sole source of truth per repo,
    // and feature sets are derived from branch-name equality. Purely additive
    // (`CREATE TABLE IF NOT EXISTS` + idempotent `ALTER`) so parallel-branch DBs
    // tolerate it. See `crate::store::ProjectStore`.
    // best-effort: idempotent additive migration: the ignore is the already-applied no-op
    let _ = conn.execute(
        "CREATE TABLE IF NOT EXISTS projects (
          project_id INTEGER PRIMARY KEY,
          name       TEXT    NOT NULL UNIQUE,
          created_at INTEGER NOT NULL,
          position   INTEGER NOT NULL DEFAULT 0
        )",
        [],
    );
    // best-effort: idempotent additive migration: the ignore is the already-applied no-op
    let _ = conn.execute("ALTER TABLE workspaces ADD COLUMN project_id INTEGER", []);
    // v56: pipeline columns on the dispatch roster. Four nullable columns, each
    // idempotent (`ALTER` fails harmlessly once the column exists), so a
    // pre-v56 DB gains them on open with every row intact — NULL everywhere,
    // which reads back as `None` and is exactly the pre-change behaviour.
    //
    //  - `stage`         which `[[pipeline.stages]]` step this row is. thegn
    //                    stores and groups by it and NEVER advances it: stage
    //                    transitions are the supervising agent's judgment (the
    //                    complement of the rejected native drain driver).
    //  - `parent_id`     the row this one was chunked out of (architect → coder
    //                    fan-out). Deliberately not a foreign key: the roster is
    //                    a cache-side ledger, and a pruned parent must never
    //                    make a child unreadable.
    //  - `session_id`    the daemon session running this dispatch — the row's
    //                    identity for pane-exit attribution when several stages
    //                    share one worktree.
    //  - `artifact_path` a POINTER to the handoff file committed in the
    //                    worktree. Git stays the source of truth; the roster
    //                    never becomes a document store (hence no meta-JSON
    //                    blob column).
    // best-effort: idempotent additive migration: the ignore is the already-applied no-op
    let _ = conn.execute("ALTER TABLE agent_dispatches ADD COLUMN stage TEXT", []);
    // best-effort: idempotent additive migration: the ignore is the already-applied no-op
    let _ = conn.execute(
        "ALTER TABLE agent_dispatches ADD COLUMN parent_id INTEGER",
        [],
    );
    // best-effort: idempotent additive migration: the ignore is the already-applied no-op
    let _ = conn.execute(
        "ALTER TABLE agent_dispatches ADD COLUMN session_id TEXT",
        [],
    );
    // best-effort: idempotent additive migration: the ignore is the already-applied no-op
    let _ = conn.execute(
        "ALTER TABLE agent_dispatches ADD COLUMN artifact_path TEXT",
        [],
    );
    // v59: `agent_dispatches.note` — the transport-retry observer's ledger
    // (THE-86). Free text written ONLY by the daemon stamper
    // (`stamp_dispatch_note`): why a headless worker died, which retry attempt
    // it reached, why a relaunch failed. Nullable everywhere; a pre-v59 row
    // reads back `None`, which is exactly the pre-change behaviour. Idempotent
    // (`ALTER` fails harmlessly once the column exists) so parallel-branch DBs
    // sharing the file tolerate it.
    let _ = conn.execute("ALTER TABLE agent_dispatches ADD COLUMN note TEXT", []);
    // v60: `agent_dispatches.chunk_path` — the chunk file a row dispatches
    // under (THE-86): a POINTER to `.thegn/pipeline/<ISSUE>/code/chunk-N.md`,
    // whose `files:` frontmatter is the row's declared scope. The scopes live
    // in the files (git is the source of truth); the roster stores pointers,
    // as always. Nullable everywhere; a pre-v60 row reads back `None`, which
    // is exactly the pre-change behaviour. Idempotent (`ALTER` fails
    // harmlessly once the column exists) so parallel-branch DBs sharing the
    // file tolerate it.
    let _ = conn.execute(
        "ALTER TABLE agent_dispatches ADD COLUMN chunk_path TEXT",
        [],
    );
    // v61: `agent_dispatches.report` — the worker's structured handoff summary
    // (verdict/commits/unverified/findings/next), ≤16 KiB, stored on the row
    // because the Lead reads it WITHOUT opening the worktree — the artifact
    // pointer (artifact_path) still points at the full document, which stays
    // git's. Nullable everywhere; a pre-v61 row reads back `None`, which is
    // exactly the pre-change behaviour. Idempotent (`ALTER` fails harmlessly
    // once the column exists) so parallel-branch DBs sharing the file tolerate
    // it.
    if !has_column(conn, "agent_dispatches", "report") {
        let _ = conn.execute("ALTER TABLE agent_dispatches ADD COLUMN report TEXT", []);
    }
    // v61 companion: per-row progress queue — a worker or monitor appends short
    // notes (≤4 KiB), read newest-last by `dispatch status`. Kept separate from
    // `agent_dispatches.note` (the daemon's transport-retry observer ledger):
    // conflating them would make every progress read re-parse for transport
    // artifacts.
    let _ = conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS agent_dispatch_notes (
           id             INTEGER PRIMARY KEY AUTOINCREMENT,
           dispatch_id    INTEGER NOT NULL,
           created_at_ms  INTEGER NOT NULL,
           text           TEXT    NOT NULL
         );
         CREATE INDEX IF NOT EXISTS idx_dispatch_notes_dispatch
           ON agent_dispatch_notes (dispatch_id, created_at_ms);",
    );
    // v63: one complete PR review snapshot per canonical worktree key. The
    // identity columns are deliberately duplicated outside the JSON so a
    // caller can reject stale feedback before presenting it.
    let _ = conn.execute(
        "CREATE TABLE IF NOT EXISTS pr_review_cache (
           worktree   TEXT PRIMARY KEY,
           branch     TEXT NOT NULL,
           pr_number  INTEGER NOT NULL,
           head_oid   TEXT NOT NULL,
           json       TEXT NOT NULL,
           fetched_at INTEGER NOT NULL
         )",
        [],
    );
}

/// v62: credential-free lineage for successful session forks. Recipes remain
/// in the live daemon entry only; this cache cannot resurrect a process.
pub(crate) fn migrate_v62(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS session_forks (
           child_id     TEXT PRIMARY KEY,
           source_kind  TEXT NOT NULL,
           source_id    TEXT NOT NULL,
           harness      TEXT,
           worktree     TEXT,
           created_at   INTEGER NOT NULL
         );
         CREATE INDEX IF NOT EXISTS idx_session_forks_source
           ON session_forks (source_kind, source_id);",
    )?;
    Ok(())
}

/// Does `table` have a column named `col`? The probe for migrations that can't
/// be expressed as an idempotent `ALTER` (a primary-key change forces a
/// rebuild-and-copy, which must run exactly once). Returns false when the table
/// doesn't exist — the base DDL will have created it in its current shape.
fn has_column(conn: &Connection, table: &str, col: &str) -> bool {
    let Ok(mut stmt) = conn.prepare(&format!("PRAGMA table_info({table})")) else {
        return false;
    };
    let Ok(rows) = stmt.query_map([], |r| r.get::<_, String>(1)) else {
        return false;
    };
    rows.flatten().any(|name| name == col)
}

/// The legacy additive ladder is intentionally best-effort for cache-only
/// tables, but v61 adds the storage contract used by the report/note commands.
/// Verify that contract before `Db::init` stamps the schema version; otherwise
/// a disk/lock/schema error swallowed by the historical ladder would make a
/// broken upgrade look complete on the next open.
pub(crate) fn verify_v61_schema(conn: &Connection) -> Result<()> {
    // Preparing the projection catches a missing `report` column without
    // reading any user payload.
    conn.prepare("SELECT report FROM agent_dispatches LIMIT 0")?;

    let notes_table: Option<String> = conn
        .query_row(
            "SELECT type FROM sqlite_master WHERE name='agent_dispatch_notes'",
            [],
            |r| r.get(0),
        )
        .optional()?;
    if notes_table.as_deref() != Some("table") {
        anyhow::bail!("schema v61 migration did not create agent_dispatch_notes");
    }

    let notes_index: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='index' AND name='idx_dispatch_notes_dispatch'",
            [],
            |r| r.get(0),
        )
        .optional()?;
    if notes_index.is_none() {
        anyhow::bail!("schema v61 migration did not create the dispatch notes index");
    }
    Ok(())
}

/// Verify the v62 cache shape before stamping the schema version. The cache is
/// intentionally metadata-only: preparing this projection also protects the
/// no-recipe contract from an incomplete upgrade.
pub(crate) fn verify_v62_schema(conn: &Connection) -> Result<()> {
    verify_v61_schema(conn)?;
    conn.prepare(
        "SELECT child_id, source_kind, source_id, harness, worktree, created_at
         FROM session_forks LIMIT 0",
    )?;
    let index: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM sqlite_master
             WHERE type='index' AND name='idx_session_forks_source'",
            [],
            |r| r.get(0),
        )
        .optional()?;
    if index.is_none() {
        anyhow::bail!("schema v62 migration did not create the session forks index");
    }
    Ok(())
}

/// Verify the v63 review-cache table after the best-effort additive ladder and
/// before `Db::init` stamps the new schema version.
pub(crate) fn verify_v63_schema(conn: &Connection) -> Result<()> {
    verify_v62_schema(conn)?;
    let table_type: Option<String> = conn
        .query_row(
            "SELECT type FROM sqlite_master WHERE name='pr_review_cache'",
            [],
            |r| r.get(0),
        )
        .optional()?;
    if table_type.as_deref() != Some("table") {
        anyhow::bail!("schema v63 migration did not create pr_review_cache");
    }
    let mut stmt = conn.prepare("PRAGMA table_info(pr_review_cache)")?;
    let columns: Vec<(String, i64, i64)> = stmt
        .query_map([], |r| Ok((r.get(1)?, r.get(3)?, r.get(5)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let expected = [
        ("worktree", 0, 1),
        ("branch", 1, 0),
        ("pr_number", 1, 0),
        ("head_oid", 1, 0),
        ("json", 1, 0),
        ("fetched_at", 1, 0),
    ];
    if columns.len() != expected.len()
        || expected.iter().any(|wanted| {
            !columns
                .iter()
                .any(|column| column.0 == wanted.0 && column.1 == wanted.1 && column.2 == wanted.2)
        })
    {
        anyhow::bail!("schema v63 pr_review_cache has an invalid shape");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::db::Db;
    use crate::store::{SessionForkStore, WorkspaceStore};

    #[test]
    fn detect_newer_schema_flags_only_a_newer_db() {
        // Older / equal on-disk versions are fine; only a strictly-newer DB
        // (written by a different-schema branch build) is flagged.
        assert_eq!(super::detect_newer_schema(5, 10), None);
        assert_eq!(super::detect_newer_schema(10, 10), None);
        assert_eq!(super::detect_newer_schema(12, 10), Some(12));
    }

    #[test]
    fn v61_schema_verifier_rejects_an_incomplete_upgrade() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE agent_dispatches (id INTEGER PRIMARY KEY, report TEXT);")
            .unwrap();
        let err = super::verify_v61_schema(&conn).unwrap_err();
        assert!(err.to_string().contains("agent_dispatch_notes"), "{err}");
    }

    #[test]
    fn pre_v63_db_gains_review_cache_without_resetting_user_data() {
        let dir = std::env::temp_dir().join(format!("thegn-mig-v63-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("thegn.db");
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE repos (path TEXT PRIMARY KEY, name TEXT NOT NULL);
                 INSERT INTO repos (path,name) VALUES ('/repo/a', 'a');
                 PRAGMA user_version = 61;",
            )
            .unwrap();
        }
        let db = Db::open_at(&path).unwrap();
        let name: String = db
            .conn()
            .query_row("SELECT name FROM repos WHERE path='/repo/a'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(name, "a");
        let snapshot = crate::review::PrReviewSnapshot {
            worktree_key: "/wt/a".into(),
            branch: "feature".into(),
            pr_number: 27,
            head_oid: "head".into(),
            ..Default::default()
        };
        crate::store::CacheStore::put_pr_review_cache(&db, &snapshot).unwrap();
        assert!(
            crate::store::CacheStore::get_pr_review_cache(&db, "/wt/a")
                .unwrap()
                .is_some()
        );
        let ver: i64 = db
            .conn()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(ver, crate::db::SCHEMA_VERSION);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn review_cache_migration_is_idempotent_and_preserves_rows() {
        let db = Db::open_memory().unwrap();
        let snapshot = crate::review::PrReviewSnapshot {
            worktree_key: "/wt/a".into(),
            branch: "feature".into(),
            pr_number: 27,
            head_oid: "head".into(),
            ..Default::default()
        };
        crate::store::CacheStore::put_pr_review_cache(&db, &snapshot).unwrap();
        super::additive_schema(db.conn());
        super::additive_schema(db.conn());
        let got = crate::store::CacheStore::get_pr_review_cache(&db, "/wt/a")
            .unwrap()
            .unwrap();
        assert_eq!(got.pr_number, 27);
        let tables: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='pr_review_cache'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(tables, 1);
    }

    #[test]
    fn pre_v62_db_gains_session_fork_lineage_cache_without_recipes() {
        let dir = std::env::temp_dir().join(format!("thegn-mig-v62-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("thegn.db");
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch("PRAGMA user_version = 61;").unwrap();
        }

        let db = Db::open_at(&path).unwrap();
        db.put_session_fork(&crate::session_fork::ForkRecord {
            child_id: "child-v62".into(),
            source_kind: crate::session_fork::ForkSourceKind::Daemon,
            source_id: "parent-v62".into(),
            harness: None,
            worktree: Some("/wt".into()),
            created_at: 62,
        })
        .unwrap();
        assert_eq!(db.session_forks().unwrap().len(), 1);
        let columns: Vec<String> = db
            .conn()
            .prepare("PRAGMA table_info(session_forks)")
            .unwrap()
            .query_map([], |r| r.get(1))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(
            columns,
            vec![
                "child_id",
                "source_kind",
                "source_id",
                "harness",
                "worktree",
                "created_at"
            ]
        );
        let ver: i64 = db
            .conn()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(ver, crate::db::SCHEMA_VERSION);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pre_v52_db_gains_calendar_tables_without_touching_user_data() {
        use crate::store::{CalendarRow, CalendarStore};
        // A v51-shaped DB carrying real user data. The migration is purely
        // additive, so both the repo row and the notification must survive —
        // thegn never resets a user's DB to pick up a schema change.
        let dir = std::env::temp_dir().join(format!("thegn-mig-v52-{}", std::process::id()));
        // best-effort: idempotent additive migration: the ignore is the already-applied no-op
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("thegn.db");
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE repos (path TEXT PRIMARY KEY, name TEXT NOT NULL);
                 INSERT INTO repos (path,name) VALUES ('/repo/a', 'a');
                 PRAGMA user_version = 51;",
            )
            .unwrap();
        }
        let db = Db::open_at(&path).unwrap();

        // The pre-existing table and its row are untouched.
        let name: String = db
            .conn()
            .query_row("SELECT name FROM repos WHERE path='/repo/a'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(name, "a");

        // The new tables exist and are writable.
        assert!(!db.has_calendar_events("work").unwrap());
        db.replace_calendar_account(
            "work",
            &[CalendarRow {
                uid: "e1".into(),
                calendar: "Work".into(),
                start_ms: 1_800_000_000_000,
                end_ms: 1_800_003_600_000,
                recurring: false,
                json: "{}".into(),
            }],
        )
        .unwrap();
        assert!(db.has_calendar_events("work").unwrap());
        let got = db
            .get_calendar_events(1_799_000_000_000, 1_801_000_000_000, &[])
            .unwrap();
        assert_eq!(got, vec![("work".to_string(), "{}".to_string())]);

        db.put_calendar_sync("work", "ics", "etag-1", 0, 0).unwrap();
        assert_eq!(
            db.get_calendar_sync("work").unwrap().unwrap().sync_token,
            "etag-1"
        );

        // And the stamp advanced.
        let ver: i64 = db
            .conn()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(ver, crate::db::SCHEMA_VERSION);
        // best-effort: idempotent additive migration: the ignore is the already-applied no-op
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pre_v56_db_gains_the_dispatch_pipeline_columns_without_resetting_anything() {
        use crate::issue::{AgentDispatchStatus as S, NewDispatch};
        use crate::store::NotificationStore;
        // A v55-shaped `agent_dispatches` (the six original scalar columns)
        // carrying a real in-flight roster row. The v56 migration is four
        // idempotent ALTERs, so the row must survive with its pipeline columns
        // reading NULL — thegn never resets a user's DB to pick up a schema
        // change.
        let dir = std::env::temp_dir().join(format!("thegn-mig-v56-{}", std::process::id()));
        // best-effort: idempotent additive migration: the ignore is the already-applied no-op
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("thegn.db");
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE agent_dispatches (
                   id               INTEGER PRIMARY KEY AUTOINCREMENT,
                   issue_id         TEXT    NOT NULL,
                   worktree_path    TEXT    NOT NULL,
                   agent_name       TEXT    NOT NULL,
                   dispatched_at_ms INTEGER NOT NULL,
                   status           TEXT    NOT NULL DEFAULT 'queued'
                 );
                 INSERT INTO agent_dispatches
                   (issue_id,worktree_path,agent_name,dispatched_at_ms,status)
                   VALUES ('linear:OLD-1','/wt/old','claude',1000,'running');
                 PRAGMA user_version = 55;",
            )
            .unwrap();
        }
        let db = Db::open_at(&path).unwrap();

        // The pre-existing row is untouched and now reads the new columns as
        // `None` — exactly the pre-change behaviour.
        let rows = db.list_dispatches().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].issue_id, "linear:OLD-1");
        assert_eq!(rows[0].status, S::Running);
        assert_eq!(rows[0].stage, None);
        assert_eq!(rows[0].parent_id, None);
        assert_eq!(rows[0].session_id, None);
        assert_eq!(rows[0].artifact_path, None);
        // A legacy row still answers the exit lookup (it is active), and the
        // new columns are writable on a fresh row.
        assert_eq!(
            db.dispatch_for_exit("/wt/old", None).unwrap(),
            Some((rows[0].id, "linear:OLD-1".to_string()))
        );
        let id = db
            .put_agent_dispatch(NewDispatch {
                stage: Some("review"),
                session_id: Some("sess-v56"),
                ..NewDispatch::new("linear:NEW-1", "/wt/old", "reviewer")
            })
            .unwrap();
        assert_eq!(
            db.get_dispatch(id).unwrap().unwrap().stage.as_deref(),
            Some("review")
        );

        // And the stamp advanced.
        let ver: i64 = db
            .conn()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(ver, crate::db::SCHEMA_VERSION);
        // best-effort: idempotent additive migration: the ignore is the already-applied no-op
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pre_v59_db_gains_the_dispatch_note_column_without_resetting_anything() {
        use crate::issue::{AgentDispatchStatus as S, NewDispatch};
        use crate::store::NotificationStore;
        // A v58-shaped `agent_dispatches` (through the v56 pipeline columns,
        // before v59's `note`) carrying a real in-flight roster row. The v59
        // migration is one idempotent ALTER, so the row must survive with the
        // new column reading NULL — thegn never resets a user's DB to pick up
        // a schema change.
        let dir = std::env::temp_dir().join(format!("thegn-mig-v59-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("thegn.db");
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE agent_dispatches (
                   id               INTEGER PRIMARY KEY AUTOINCREMENT,
                   issue_id         TEXT    NOT NULL,
                   worktree_path    TEXT    NOT NULL,
                   agent_name       TEXT    NOT NULL,
                   dispatched_at_ms INTEGER NOT NULL,
                   status           TEXT    NOT NULL DEFAULT 'queued',
                   stage            TEXT,
                   parent_id        INTEGER,
                   session_id       TEXT,
                   artifact_path    TEXT
                 );
                 INSERT INTO agent_dispatches
                   (issue_id,worktree_path,agent_name,dispatched_at_ms,status,stage)
                   VALUES ('linear:OLD-2','/wt/old','claude',1000,'running','code');
                 PRAGMA user_version = 58;",
            )
            .unwrap();
        }
        let db = Db::open_at(&path).unwrap();

        // The pre-existing row is untouched and now reads the new column as
        // `None` — exactly the pre-change behaviour.
        let rows = db.list_dispatches().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].issue_id, "linear:OLD-2");
        assert_eq!(rows[0].status, S::Running);
        assert_eq!(rows[0].note, None);

        // The new column is writable on a fresh row and stamps back onto the
        // migrated one (the observer's write path).
        let id = db
            .put_agent_dispatch(NewDispatch {
                stage: Some("review"),
                ..NewDispatch::new("linear:NEW-2", "/wt/old", "reviewer")
            })
            .unwrap();
        db.stamp_dispatch_note(rows[0].id, "transport: connection error. (attempt 1/3)")
            .unwrap();
        assert_eq!(
            db.get_dispatch(rows[0].id)
                .unwrap()
                .unwrap()
                .note
                .as_deref(),
            Some("transport: connection error. (attempt 1/3)")
        );
        assert_eq!(db.get_dispatch(id).unwrap().unwrap().note, None);

        // And the stamp advanced.
        let ver: i64 = db
            .conn()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(ver, crate::db::SCHEMA_VERSION);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pre_v60_db_gains_the_dispatch_chunk_path_column_without_resetting_anything() {
        use crate::issue::{AgentDispatchStatus as S, NewDispatch};
        use crate::store::NotificationStore;
        // A v59-shaped `agent_dispatches` (through v59's `note`, before
        // v60's `chunk_path`) carrying a real in-flight roster row. The v60
        // migration is one idempotent ALTER, so the row must survive with the
        // new column reading NULL — thegn never resets a user's DB to pick up
        // a schema change.
        let dir = std::env::temp_dir().join(format!("thegn-mig-v60-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("thegn.db");
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE agent_dispatches (
                   id               INTEGER PRIMARY KEY AUTOINCREMENT,
                   issue_id         TEXT    NOT NULL,
                   worktree_path    TEXT    NOT NULL,
                   agent_name       TEXT    NOT NULL,
                   dispatched_at_ms INTEGER NOT NULL,
                   status           TEXT    NOT NULL DEFAULT 'queued',
                   stage            TEXT,
                   parent_id        INTEGER,
                   session_id       TEXT,
                   artifact_path    TEXT,
                   note             TEXT
                 );
                 INSERT INTO agent_dispatches
                   (issue_id,worktree_path,agent_name,dispatched_at_ms,status,stage,note)
                   VALUES ('linear:OLD-3','/wt/old','claude',1000,'running','code',
                           'transport: connection error. (attempt 1/3)');
                 PRAGMA user_version = 59;",
            )
            .unwrap();
        }
        let db = Db::open_at(&path).unwrap();

        // The pre-existing row is untouched and now reads the new column as
        // `None` — exactly the pre-change behaviour, note included.
        let rows = db.list_dispatches().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].issue_id, "linear:OLD-3");
        assert_eq!(rows[0].status, S::Running);
        assert_eq!(
            rows[0].note.as_deref(),
            Some("transport: connection error. (attempt 1/3)")
        );
        assert_eq!(rows[0].chunk_path, None);

        // The new column is writable on a fresh row (the roster records the
        // pointer, never the payload).
        let id = db
            .put_agent_dispatch(NewDispatch {
                stage: Some("review"),
                ..NewDispatch::new("linear:NEW-3", "/wt/old", "reviewer")
            })
            .unwrap();
        assert_eq!(db.get_dispatch(id).unwrap().unwrap().chunk_path, None);

        // And the stamp advanced.
        let ver: i64 = db
            .conn()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(ver, crate::db::SCHEMA_VERSION);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pre_v61_db_gains_report_column_and_notes_table_without_resetting_anything() {
        use crate::issue::AgentDispatchStatus as S;
        use crate::store::NotificationStore;
        // A v60-shaped `agent_dispatches` (through v60's `chunk_path`, before
        // v61's `report` + `agent_dispatch_notes`) carrying a real in-flight
        // roster row. The v61 migration is one idempotent ALTER plus a
        // CREATE TABLE IF NOT EXISTS, so the row must survive with the new
        // column reading NULL and notes must be appendable.
        let dir = std::env::temp_dir().join(format!("thegn-mig-v61-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("thegn.db");
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE agent_dispatches (
                   id               INTEGER PRIMARY KEY AUTOINCREMENT,
                   issue_id         TEXT    NOT NULL,
                   worktree_path    TEXT    NOT NULL,
                   agent_name       TEXT    NOT NULL,
                   dispatched_at_ms INTEGER NOT NULL,
                   status           TEXT    NOT NULL DEFAULT 'queued',
                   stage            TEXT,
                   parent_id        INTEGER,
                   session_id       TEXT,
                   artifact_path    TEXT,
                   note             TEXT,
                   chunk_path       TEXT
                 );
                 INSERT INTO agent_dispatches
                   (issue_id,worktree_path,agent_name,dispatched_at_ms,status,stage,note,chunk_path)
                   VALUES ('linear:OLD-4','/wt/old','claude',1000,'running','code',
                           'transport: retry 2/3', 'code/chunk-1.md');
                 PRAGMA user_version = 60;",
            )
            .unwrap();
        }
        let db = Db::open_at(&path).unwrap();

        // The pre-existing row is untouched and now reads the new column as
        // `None` — exactly the pre-change behaviour.
        let rows = db.list_dispatches().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].issue_id, "linear:OLD-4");
        assert_eq!(rows[0].status, S::Running);
        assert_eq!(rows[0].note.as_deref(), Some("transport: retry 2/3"));
        assert_eq!(rows[0].chunk_path.as_deref(), Some("code/chunk-1.md"));
        assert_eq!(rows[0].report, None);

        // The notes table is empty but writable.
        let note_id = db
            .append_dispatch_note(rows[0].id, "stage progress: linting")
            .unwrap();
        assert!(note_id > 0);
        let notes = db.dispatch_notes(rows[0].id, None, 0).unwrap();
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].text, "stage progress: linting");

        // The report column is writable.
        db.set_dispatch_report(rows[0].id, "verdict: done").unwrap();
        let reloaded = db.get_dispatch(rows[0].id).unwrap().unwrap();
        assert_eq!(reloaded.report.as_deref(), Some("verdict: done"));

        // And the stamp advanced.
        let ver: i64 = db
            .conn()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(ver, crate::db::SCHEMA_VERSION);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn calendar_cache_round_trips_and_scopes_by_account() {
        use crate::store::{CalendarRow, CalendarStore};
        let dir = std::env::temp_dir().join(format!("thegn-cal-db-{}", std::process::id()));
        // best-effort: idempotent additive migration: the ignore is the already-applied no-op
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = Db::open_at(&dir.join("thegn.db")).unwrap();

        let row = |uid: &str, start: i64, recurring: bool| CalendarRow {
            uid: uid.into(),
            calendar: String::new(),
            start_ms: start,
            end_ms: start + 3_600_000,
            recurring,
            json: format!("{{\"uid\":\"{uid}\"}}"),
        };
        db.replace_calendar_account("a", &[row("a1", 1_000_000, false)])
            .unwrap();
        db.replace_calendar_account("b", &[row("b1", 1_000_000, false)])
            .unwrap();

        // The account filter scopes the query.
        let only_a = db
            .get_calendar_events(0, 9_000_000, &["a".to_string()])
            .unwrap();
        assert_eq!(only_a.len(), 1);
        assert_eq!(only_a[0].0, "a");
        // An empty filter means every account.
        assert_eq!(db.get_calendar_events(0, 9_000_000, &[]).unwrap().len(), 2);

        // A recurrence master is returned even when its own span misses the
        // window — an old DTSTART still generates today's occurrences.
        db.put_calendar_events("a", &[row("master", 0, true)])
            .unwrap();
        let far_future = db
            .get_calendar_events(500_000_000_000, 500_000_100_000, &[])
            .unwrap();
        assert_eq!(far_future.len(), 1, "only the master: {far_future:?}");
        assert!(far_future[0].1.contains("master"));

        // A one-shot event outside the window is not.
        assert!(
            !db.get_calendar_events(500_000_000_000, 500_000_100_000, &[])
                .unwrap()
                .iter()
                .any(|(_, j)| j.contains("a1"))
        );

        // Upsert replaces rather than duplicating.
        db.put_calendar_events("a", &[row("a1", 2_000_000, false)])
            .unwrap();
        let a_rows = db
            .get_calendar_events(0, 9_000_000, &["a".to_string()])
            .unwrap();
        assert_eq!(a_rows.len(), 2, "a1 + master, not three rows");

        // Tombstones delete only the named uids, only in that account.
        db.delete_calendar_events("a", &["a1".to_string()]).unwrap();
        assert!(
            !db.get_calendar_events(0, 9_000_000, &["a".to_string()])
                .unwrap()
                .iter()
                .any(|(_, j)| j.contains("a1"))
        );
        assert!(db.has_calendar_events("b").unwrap(), "b is untouched");

        // A full replace clears the account's prior rows, master included.
        db.replace_calendar_account("a", &[row("a2", 3_000_000, false)])
            .unwrap();
        let a_rows = db
            .get_calendar_events(0, 9_000_000, &["a".to_string()])
            .unwrap();
        assert_eq!(a_rows.len(), 1);
        assert!(a_rows[0].1.contains("a2"));

        // best-effort: idempotent additive migration: the ignore is the already-applied no-op
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn calendar_sync_errors_do_not_disturb_the_cached_events() {
        use crate::store::{CalendarRow, CalendarStore};
        // THE don't-clobber rule at the storage layer: recording a failure must
        // leave both the events and the resume cursor alone.
        let dir = std::env::temp_dir().join(format!("thegn-cal-err-{}", std::process::id()));
        // best-effort: idempotent additive migration: the ignore is the already-applied no-op
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = Db::open_at(&dir.join("thegn.db")).unwrap();

        db.replace_calendar_account(
            "work",
            &[CalendarRow {
                uid: "e1".into(),
                calendar: String::new(),
                start_ms: 1_000_000,
                end_ms: 2_000_000,
                recurring: false,
                json: "{}".into(),
            }],
        )
        .unwrap();
        db.put_calendar_sync("work", "ics_url", "etag-1", 10, 20)
            .unwrap();

        db.set_calendar_error("work", "connection refused").unwrap();
        let sync = db.get_calendar_sync("work").unwrap().unwrap();
        assert_eq!(sync.last_error, "connection refused");
        assert_eq!(sync.sync_token, "etag-1", "the resume cursor survives");
        assert!(db.has_calendar_events("work").unwrap(), "events survive");

        // A later success clears the error, so a one-off blip isn't reported
        // forever.
        db.put_calendar_sync("work", "ics_url", "etag-2", 10, 20)
            .unwrap();
        let sync = db.get_calendar_sync("work").unwrap().unwrap();
        assert!(sync.last_error.is_empty());
        assert_eq!(sync.sync_token, "etag-2");

        // An error for an account with no prior sync row still records.
        db.set_calendar_error("fresh", "boom").unwrap();
        assert_eq!(
            db.get_calendar_sync("fresh").unwrap().unwrap().last_error,
            "boom"
        );
        assert!(db.get_calendar_sync("never-seen").unwrap().is_none());

        // best-effort: idempotent additive migration: the ignore is the already-applied no-op
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pruning_spares_recurrence_masters() {
        use crate::store::{CalendarRow, CalendarStore};
        let dir = std::env::temp_dir().join(format!("thegn-cal-prune-{}", std::process::id()));
        // best-effort: idempotent additive migration: the ignore is the already-applied no-op
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = Db::open_at(&dir.join("thegn.db")).unwrap();

        let row = |uid: &str, recurring: bool| CalendarRow {
            uid: uid.into(),
            calendar: String::new(),
            start_ms: 1_000,
            end_ms: 2_000,
            recurring,
            json: format!("{{\"uid\":\"{uid}\"}}"),
        };
        db.replace_calendar_account("a", &[row("old", false), row("weekly", true)])
            .unwrap();

        let removed = db.prune_calendar_events(10_000).unwrap();
        assert_eq!(removed, 1, "only the finished one-shot event");
        let left = db.get_calendar_events(0, 500_000, &[]).unwrap();
        assert_eq!(left.len(), 1);
        assert!(
            left[0].1.contains("weekly"),
            "an old DTSTART still generates today's occurrences"
        );
        // best-effort: idempotent additive migration: the ignore is the already-applied no-op
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pre_v50_attention_acks_gain_episode_and_composite_key() {
        use crate::attention::{AttentionAck, AttentionReason};
        use crate::store::NotificationStore;
        // A v49-shaped attention_acks table (worktree_path PRIMARY KEY, no
        // `episode`) with a real ack in it. The rebuild must COPY that row, not
        // drop it: an ack the user already made must not re-nag just because the
        // app upgraded.
        let dir = std::env::temp_dir().join(format!("thegn-mig-v50-{}", std::process::id()));
        // best-effort: idempotent additive migration: the ignore is the already-applied no-op
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("thegn.db");
        let reason = serde_json::to_string(&AttentionReason::CiFailed).unwrap();
        // A recent stamp: `Db::open` also runs the 90-day size sweep, and an
        // ancient row is correctly reclaimed by it rather than migrated.
        let acked_at = crate::util::now() - 3600;
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(&format!(
                "CREATE TABLE attention_acks (
                   worktree_path TEXT PRIMARY KEY, reason TEXT NOT NULL,
                   since INTEGER, acked_at INTEGER);
                 INSERT INTO attention_acks (worktree_path,reason,since,acked_at)
                   VALUES ('/wt/a', '{reason}', NULL, {acked_at}),
                          ('/wt/ancient', '{reason}', NULL, 77);
                 PRAGMA user_version = 49;"
            ))
            .unwrap();
        }
        let db = Db::open_at(&path).unwrap();
        let rows = db.list_attention_acks().unwrap();
        assert_eq!(
            rows.len(),
            1,
            "the pre-v50 ack must survive the rebuild (and only the ancient row \
             is swept)"
        );
        assert_eq!(rows[0].worktree_path, "/wt/a");
        assert_eq!(rows[0].since, None);
        assert_eq!(rows[0].episode, 0, "migrated rows carry the 0 sentinel");
        assert_eq!(
            rows[0].acked_at, acked_at,
            "the original stamp is preserved"
        );
        // And it still suppresses the signal it was made against.
        let migrated = AttentionAck {
            reason: AttentionReason::CiFailed,
            since: None,
            episode: 0,
        };
        assert_eq!(
            serde_json::from_str::<AttentionReason>(&rows[0].reason).unwrap(),
            migrated.reason
        );

        // The composite key is live: a second reason on the same worktree now
        // coexists instead of overwriting the first (the pre-v50 behavior that
        // made acking the most-urgent signal destroy the ack for the one it
        // outranked).
        let other = serde_json::to_string(&AttentionReason::AgentNeedsInput).unwrap();
        db.put_attention_ack("/wt/a", &other, Some(5), 0).unwrap();
        assert_eq!(db.list_attention_acks().unwrap().len(), 2);
        // UPSERT still replaces same-(worktree, reason) with the newer episode.
        db.put_attention_ack("/wt/a", &reason, None, 9).unwrap();
        let rows = db.list_attention_acks().unwrap();
        assert_eq!(rows.len(), 2, "UPSERT, not a third row");
        assert_eq!(
            rows.iter().find(|r| r.reason == reason).map(|r| r.episode),
            Some(9)
        );
        // Targeted delete drops one reason and leaves the other.
        db.delete_attention_ack("/wt/a", Some(&reason)).unwrap();
        let rows = db.list_attention_acks().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].reason, other);
        // Un-targeted delete clears the worktree (the `del_worktree` cascade).
        db.delete_attention_ack("/wt/a", None).unwrap();
        assert!(db.list_attention_acks().unwrap().is_empty());
        // best-effort: idempotent additive migration: the ignore is the already-applied no-op
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pre_v49_merge_queue_gains_agent_attempts_defaulting_to_zero() {
        use crate::store::WorktreeAuxStore;
        // A merge_queue table shaped like v48 (no agent_attempts), with a row in
        // it, must survive the additive migration and read back as 0 attempts —
        // which is the pre-change behavior for that row's next drain.
        let dir = std::env::temp_dir().join(format!("thegn-mig-v49-{}", std::process::id()));
        // best-effort: idempotent additive migration: the ignore is the already-applied no-op
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("thegn.db");
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE merge_queue (
                   worktree TEXT PRIMARY KEY, branch TEXT NOT NULL,
                   target_branch TEXT NOT NULL, status TEXT NOT NULL DEFAULT 'queued',
                   queued_at INTEGER NOT NULL, updated_at INTEGER NOT NULL,
                   result_oid TEXT, conflict_paths TEXT, error_detail TEXT, location TEXT);
                 INSERT INTO merge_queue
                   (worktree,branch,target_branch,status,queued_at,updated_at)
                   VALUES ('/wt/a','feat','main','gate_failed',1,1);
                 PRAGMA user_version = 48;",
            )
            .unwrap();
        }
        let db = Db::open_at(&path).unwrap();
        let rows = db.list_merge_queue().unwrap();
        assert_eq!(rows.len(), 1, "the pre-v49 row must survive");
        assert_eq!(rows[0].branch, "feat");
        assert_eq!(rows[0].agent_attempts, 0);
        // And the new column is writable on that migrated row.
        db.set_merge_agent_attempts("/wt/a", 2).unwrap();
        assert_eq!(db.list_merge_queue().unwrap()[0].agent_attempts, 2);
        // `retry` clears both the status and the budget.
        assert!(db.retry_merge_entry("/wt/a").unwrap());
        let r = &db.list_merge_queue().unwrap()[0];
        assert_eq!(r.status, "queued");
        assert_eq!(r.agent_attempts, 0);
        assert!(!db.retry_merge_entry("/wt/nope").unwrap());
        // best-effort: idempotent additive migration: the ignore is the already-applied no-op
        let _ = std::fs::remove_dir_all(&dir);
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
