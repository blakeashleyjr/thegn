//! SQLite-backed state & history (replaces the old JSON files).
//!
//! One global DB at `$XDG_STATE_HOME/thegn/thegn.db`:
//!   repos      — every repo ever opened (the launcher's "recents")
//!   workspaces — a repo opened as a zellij session (one session per repo)
//!   worktrees  — thegn-managed worktrees (one per zellij tab; keyed by path)
//!
//! git is the source of truth for worktrees on disk, and live `zellij
//! list-sessions` for sessions; this is a cache + history layer. rusqlite is
//! bundled, so there's no system sqlite dependency.

use crate::util;
use anyhow::Result;
use rusqlite::Connection;
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
/// v13: adds the LLM-proxy state tables — `proxy_health` (exhaustion markers,
/// replacing the Go proxy's `health.json`), `proxy_requests` (per-request
/// audit/query log; never stores prompt/completion bodies), `proxy_virtual_keys`
/// (per-agent keys → upstream binding + scope), and `proxy_budgets` (per-scope
/// spend + caps). Also formalizes the already-present `container_events` /
/// `layouts` tables under this version.
/// v14: adds `group_tabs.pane_cwds` (per-leaf working directories) so
/// resurrected panes respawn where they last were.
/// v15: adds `group_tabs.pane_cmds` (per-leaf last foreground command, JSON
/// `pane id → {argv, cwd}`) so a resurrected pane can offer to relaunch the
/// program that was running after a crash or full restart.
/// v16: adds `workspaces.position` — a persistent per-workspace sort key, the
/// source of truth for sidebar workspace order (was recency). Backfilled from
/// the prior `last_active DESC` order so the first launch after upgrade looks
/// unchanged; thereafter order is manual (Ctrl+Alt+↑/↓) and stable.
/// v20: adds `worktree_disk` (size caches: disk badges, warning, `thegn disk`).
/// v22: adds `merge_queue` (fold-actor queue/results; `thegn integrate`).
/// v23: adds `group_tabs.pane_sessions` (per-leaf provider exec session JSON,
/// so native-exec panes reattach to their live remote session on restart).
/// v24: adds `forwards` (the resurrection layer for auto port forwards, `[forward]`).
/// v27: adds `registers` (persisted vim yank registers; `"+` never persisted).
/// v28: re-keys `my_work_cache` to per-scope rows (repo root, or `"*"` for all).
/// v29: adds `group_tabs.scrollback_snapshot` (per-leaf captured scrollback tail,
/// JSON `pane id → text`) so a resurrected pane repaints its recent history
/// instead of a blank screen. Additive; absent/NULL on pre-v29 rows = no history.
/// v30: adds `hosts` + `host_inventory` + `host_events` (see [`crate::host_db`]).
/// v31: adds `loc_cache.report_json` (per-language tokei breakdown; [`crate::loc`]).
/// v32: adds `repo_trust` (TOFU approvals for repo overlays; [`crate::repo_trust`]).
/// v33: adds `zones` + `workspaces.zone_id` ([`crate::zone`]). `pub` for host-side
/// schema-mismatch messaging.
/// v34: adds `host_capacity`/`host_tenancy`/`placement_health`/`placement_events`
/// (the placement engine; see [`crate::db_placement`]).
/// v35: `hosts` gains `headroom_json`/`last_headroom` (the measured layer).
/// v36: adds `compute_budgets`/`compute_meters` (see [`crate::db_compute`]).
/// v37: adds `intents` (the CLI→compositor mailbox behind `thegn open`;
/// see [`crate::store::IntentStore`]).
/// v38: adds `iroh_tokens` (per-sandbox auth tokens for the iroh call-home reach;
/// see `crate::db_iroh`).
/// v39: adds `worktree_hibernations` (snapshot-then-destroy bookkeeping; see
/// [`crate::store::HibernationStore`] — DDL in `db_migrate`).
/// v40: adds `daemons`/`session_leases`/`pairings` (the control-plane registry;
/// see [`crate::store::ControlStore`] — DDL in `db_control`).
pub const SCHEMA_VERSION: i64 = 42;

pub struct Db {
    conn: Connection,
    /// On-disk `user_version` when newer than [`SCHEMA_VERSION`] (a newer build wrote this shared file), else `None`.
    pub(crate) schema_mismatch: Option<i64>,
}

impl Db {
    /// Connection accessor for sibling `impl Db` query modules (`conn` stays private).
    pub(crate) fn conn(&self) -> &Connection {
        &self.conn
    }
}

/// One row of the local merge queue (`[merge_queue]`, v22). Keyed by worktree;
/// `status` is one of queued/folding/verifying/landed/deferred/gate_failed/
/// agent_running/ready/needs_human.
/// `conflict_paths` is newline-joined when present.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct MergeQueueRow {
    pub worktree: String,
    pub branch: String,
    pub target_branch: String,
    pub status: String,
    pub queued_at: i64,
    pub updated_at: i64,
    pub result_oid: Option<String>,
    pub conflict_paths: Option<String>,
    pub error_detail: Option<String>,
}

// Share/forward resurrection rows live in `models` (size-capped file); the
// `crate::db::{ShareRow, ForwardRow}` paths stay valid via this re-export.
pub use crate::models::{ForwardRow, ShareRow};

/// A persisted LLM-proxy exhaustion marker (one per backend+model).
#[derive(Debug, Clone)]
pub struct ProxyHealthRow {
    pub backend: String,
    pub model: String,
    pub kind: String,
    pub reason: String,
    pub since_ms: i64,
    pub next_probe_ms: i64,
    pub is_stale: bool,
    pub consecutive_failures: i64,
    pub cred_file: Option<String>,
    pub cred_mtime_ms: Option<i64>,
}

/// A per-request audit row for the proxy. Carries only routing/usage/cost
/// metadata — never prompt or completion bodies.
#[derive(Debug, Clone, Default)]
pub struct ProxyRequestRow {
    pub ts_ms: i64,
    pub protocol: String,
    pub route: String,
    pub virtual_key: Option<String>,
    pub agent: Option<String>,
    pub worktree: Option<String>,
    pub workspace: Option<String>,
    pub client_model: String,
    pub backend: String,
    pub backend_model: String,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cost_usd: f64,
    pub cost_source: String,
    pub outcome: String,
    pub error_code: Option<String>,
}

/// A per-scope spend/budget row.
#[derive(Debug, Clone)]
pub struct ProxyBudgetRow {
    pub scope: String,
    pub period: String,
    pub spent_tokens: i64,
    pub spent_cost: f64,
    pub limit_tokens: Option<i64>,
    pub limit_cost: Option<f64>,
    pub reset_ms: i64,
    pub killed: bool,
}

/// One pre-provisioned spare in the warm pool (`pool_spares`). A spare is created
/// generically (not bound to a worktree), fully provisioned + checkpointed, then
/// `claimed` by a new worktree which binds it via `worktrees.provider_sandbox_id`.
#[derive(Debug, Clone)]
pub struct PoolSpare {
    pub sandbox_name: String,
    pub repo_path: String,
    pub env_name: String,
    /// `"provisioning"` | `"ready"` | `"claimed"`.
    pub state: String,
    pub checkpoint_id: Option<String>,
    pub lock_hash: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

fn db_path() -> PathBuf {
    util::xdg_state_home().join("thegn/thegn.db")
}

/// The current session marker (the repo path the host runs against, or "default"
/// when unset). Recorded on worktree rows; the native host keys workspaces by
/// repo path, so this is a coarse fallback only.
pub fn session() -> String {
    std::env::var("THEGN_SESSION").unwrap_or_else(|_| "default".into())
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
        // v28: `my_work_cache` re-keyed from a single `id=0` row to per-`scope`
        // rows. It's a pure cache (rebuilt by the background worker), so drop the
        // old-shape table here; the CREATE below recreates it with the new shape
        // and the next refresh repopulates it.
        if ver < 28 {
            let _ = conn.execute("DROP TABLE IF EXISTS my_work_cache", []);
        }
        if ver < SCHEMA_VERSION {
            conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        }
        // A newer-schema DB (different branch sharing this file): warn + tolerate.
        let schema_mismatch = crate::db_migrate::detect_newer_schema(ver, SCHEMA_VERSION);

        let _ = conn.execute("ALTER TABLE worktrees ADD COLUMN sandbox_backend TEXT", []);
        // v31: per-language LOC report JSON alongside the total (idempotent).
        let _ = conn.execute("ALTER TABLE loc_cache ADD COLUMN report_json TEXT", []);

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
              last_active  INTEGER,
              env_name     TEXT
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
              sandbox_backend TEXT,
              env_name     TEXT
            );
            CREATE TABLE IF NOT EXISTS pr_cache (
              worktree   TEXT PRIMARY KEY,
              branch     TEXT,
              json       TEXT,
              fetched_at INTEGER
            );
            -- CI run-history cache per worktree (TTL'd JSON `Vec<ci::CiRun>`),
            -- so the CI panel/view paint instantly from cache then hydrate live
            -- off the loop — exactly like `pr_cache` (AV group).
            CREATE TABLE IF NOT EXISTS ci_runs_cache (
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
              worktree    TEXT PRIMARY KEY,
              loc         INTEGER,
              report_json TEXT,
              fetched_at  INTEGER
            );
            -- v20: per-worktree disk usage (bytes). `size_bytes` is the whole
            -- checkout, `target_bytes` the `target/` subtree. Populated by an
            -- off-loop background scan; the UI paints sizes from this cache so
            -- the (seconds-long) `du` never touches the event/hydration loop.
            CREATE TABLE IF NOT EXISTS worktree_disk (
              worktree     TEXT PRIMARY KEY,
              size_bytes   INTEGER,
              target_bytes INTEGER,
              fetched_at   INTEGER
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
              pane_cwds    TEXT,
              pane_cmds    TEXT,
              pane_sessions TEXT,
              scrollback_snapshot TEXT,
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
            -- v18 / v28: the unified "My Work" feed of `Vec<WorkRow>` JSON —
            -- assigned issues (all providers), review-requested / authored PRs,
            -- and high-priority notifications. v28 re-keys it by `scope`: the
            -- active repo's root path for the default (repo-scoped) feed, or `"*"`
            -- for the cross-repo "all" toggle. Refreshed on a background worker.
            CREATE TABLE IF NOT EXISTS my_work_cache (
              scope      TEXT    PRIMARY KEY,
              json       TEXT    NOT NULL,
              fetched_at INTEGER NOT NULL
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
            -- v41: per-worktree acknowledgement of a "Needs you" attention
            -- signal. Stores the exact (reason, since) that was showing when the
            -- user quieted it, so the nag stays silenced for *that episode* only
            -- (a changed reason / advanced `since` re-fires — see
            -- `attention::AttentionScore::is_acked_by`). Purely additive cache;
            -- git / live state is truth, so a stale row just re-nags harmlessly.
            CREATE TABLE IF NOT EXISTS attention_acks (
              worktree_path TEXT PRIMARY KEY,
              reason        TEXT    NOT NULL,
              since         INTEGER,
              acked_at      INTEGER
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
            -- Named, reusable pane-layout snapshots (items 99/115): an abstract
            -- LayoutSpec (splits + per-leaf programs) serialized to JSON, recalled
            -- by name from the palette or applied as a worktree-template layout.
            CREATE TABLE IF NOT EXISTS layouts (
              name       TEXT PRIMARY KEY,
              spec       TEXT NOT NULL,
              created_at INTEGER NOT NULL
            );
            -- v13: LLM-proxy state. The proxy daemon is the single chokepoint all
            -- agent model traffic crosses; these tables replace the Go proxy's
            -- flat files (health.json, queries.jsonl) and add per-agent identity
            -- + budget machinery the Go version never had.
            --
            -- proxy_health: one exhaustion marker per (backend, model). Survives
            -- restarts so a cooled-down backend isn't re-hammered immediately.
            CREATE TABLE IF NOT EXISTS proxy_health (
              backend              TEXT    NOT NULL,
              model                TEXT    NOT NULL,
              kind                 TEXT    NOT NULL,
              reason               TEXT    NOT NULL DEFAULT '',
              since_ms             INTEGER NOT NULL,
              next_probe_ms        INTEGER NOT NULL,
              is_stale             INTEGER NOT NULL DEFAULT 0,
              consecutive_failures INTEGER NOT NULL DEFAULT 0,
              cred_file            TEXT,
              cred_mtime_ms        INTEGER,
              PRIMARY KEY (backend, model)
            );
            -- proxy_requests: per-request audit/query log. NEVER stores prompt or
            -- completion bodies — only routing/usage/cost metadata (preserves the
            -- Go proxy's privacy invariant). virtual_key/agent/worktree/workspace
            -- carry the resolved caller identity for spend attribution.
            CREATE TABLE IF NOT EXISTS proxy_requests (
              id            INTEGER PRIMARY KEY AUTOINCREMENT,
              ts_ms         INTEGER NOT NULL,
              protocol      TEXT    NOT NULL DEFAULT 'openai',
              route         TEXT    NOT NULL DEFAULT '',
              virtual_key   TEXT,
              agent         TEXT,
              worktree      TEXT,
              workspace     TEXT,
              client_model  TEXT    NOT NULL DEFAULT '',
              backend       TEXT    NOT NULL DEFAULT '',
              backend_model TEXT    NOT NULL DEFAULT '',
              input_tokens  INTEGER NOT NULL DEFAULT 0,
              output_tokens INTEGER NOT NULL DEFAULT 0,
              cost_usd      REAL    NOT NULL DEFAULT 0,
              cost_source   TEXT    NOT NULL DEFAULT 'unknown',
              outcome       TEXT    NOT NULL DEFAULT '',
              error_code    TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_proxy_requests_ts
              ON proxy_requests (ts_ms DESC);
            CREATE INDEX IF NOT EXISTS idx_proxy_requests_scope
              ON proxy_requests (agent, worktree, ts_ms DESC);
            -- proxy_virtual_keys: per-agent tokens. The proxy holds the real
            -- upstream credentials; agents authenticate with a virtual key that
            -- resolves to a caller identity (scope) + upstream binding (V 287).
            CREATE TABLE IF NOT EXISTS proxy_virtual_keys (
              key_id       TEXT PRIMARY KEY,
              token_hash   TEXT NOT NULL,
              label        TEXT NOT NULL DEFAULT '',
              scope        TEXT NOT NULL DEFAULT 'global',
              upstream     TEXT,
              created_at   INTEGER NOT NULL,
              revoked_at   INTEGER
            );
            -- proxy_budgets: per-scope spend + caps (V 292/295). scope is one of
            -- 'global', 'worktree:<path>', 'agent:<name>'. A null limit means no
            -- cap; reset_ms anchors the rolling daily/weekly/monthly window.
            CREATE TABLE IF NOT EXISTS proxy_budgets (
              scope          TEXT PRIMARY KEY,
              period         TEXT NOT NULL DEFAULT 'monthly',
              spent_tokens   INTEGER NOT NULL DEFAULT 0,
              spent_cost     REAL    NOT NULL DEFAULT 0,
              limit_tokens   INTEGER,
              limit_cost     REAL,
              reset_ms       INTEGER NOT NULL DEFAULT 0,
              killed         INTEGER NOT NULL DEFAULT 0
            );
            -- accounts: thegn-managed coding-agent credential homes for
            -- client-side account switching (item 656). Config `[[accounts]]`
            -- entries are merged in read-only at the call site; this table holds
            -- accounts created by the in-app "Add account" login flow. `dir` is
            -- the credential home (CODEX_HOME / CLAUDE_CONFIG_DIR); `managed` is 1
            -- when thegn owns the dir. Active-account pointers live in ui_state
            -- under scope `account:<provider>[:ws:<slug>|:wt:<path>]`.
            CREATE TABLE IF NOT EXISTS accounts (
              provider   TEXT    NOT NULL,
              name       TEXT    NOT NULL,
              dir        TEXT    NOT NULL,
              managed    INTEGER NOT NULL DEFAULT 1,
              created_at INTEGER NOT NULL,
              last_used  INTEGER,
              PRIMARY KEY (provider, name)
            );
            -- merge_queue (v22): the local fold-actor's queue + result cache,
            -- keyed by worktree. Git stays the source of truth; this is the UI
            -- feed and the durable record of what landed / what was deferred.
            -- status: queued | folding | verifying | landed | deferred | gate_failed
            CREATE TABLE IF NOT EXISTS merge_queue (
              worktree       TEXT PRIMARY KEY,
              branch         TEXT NOT NULL,
              target_branch  TEXT NOT NULL,
              status         TEXT NOT NULL DEFAULT 'queued',
              queued_at      INTEGER NOT NULL,
              updated_at     INTEGER NOT NULL,
              result_oid     TEXT,
              conflict_paths TEXT,
              error_detail   TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_merge_queue_status
              ON merge_queue (status, queued_at);
            -- env_base_snapshots (v25): a per-(repo, env) provider snapshot that
            -- already has the repo's nix devShell built, so a NEW worktree-sprite is
            -- created FROM it (instant) instead of rebuilding the toolchain. Keyed
            -- with the flake.lock hash so a lockfile change invalidates the base.
            CREATE TABLE IF NOT EXISTS env_base_snapshots (
              repo_path    TEXT NOT NULL,
              env_name     TEXT NOT NULL,
              snapshot_id  TEXT NOT NULL,
              lock_hash    TEXT NOT NULL,
              updated_at   INTEGER NOT NULL,
              PRIMARY KEY (repo_path, env_name)
            );
            -- intents (v37): the CLI→compositor mailbox (`thegn open`).
            -- Same pattern as notifications: a CLI process writes a row, the
            -- live compositor's model refresh claims-and-deletes it (~1s).
            -- No IPC by design.
            CREATE TABLE IF NOT EXISTS intents (
              id         INTEGER PRIMARY KEY AUTOINCREMENT,
              kind       TEXT    NOT NULL,
              payload    TEXT    NOT NULL,
              created_at INTEGER NOT NULL
            );
            -- semantic blast-radius graph (v42, items 313/316): the inter-entity
            -- impact graph, sourced from LSP `references` off the event loop.
            -- Pure derived state — a fresh DB rebuilds it from the fs-watcher, so
            -- no backfill on upgrade. `file` is the absolute worktree path;
            -- `id` = hash(repo, file, name, kind); `span` is "start-end" (1-based
            -- inclusive lines); `source_hash` is the file source at parse time.
            CREATE TABLE IF NOT EXISTS sem_entity (
              id          TEXT PRIMARY KEY,
              file        TEXT NOT NULL,
              name        TEXT NOT NULL,
              kind        TEXT NOT NULL,
              span        TEXT NOT NULL,
              source_hash TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_sem_entity_file ON sem_entity (file);
            -- sem_edge: caller (src) → callee (dst). kind: 'ref' | 'call' | 'test'.
            CREATE TABLE IF NOT EXISTS sem_edge (
              src_id TEXT NOT NULL,
              dst_id TEXT NOT NULL,
              kind   TEXT NOT NULL,
              PRIMARY KEY (src_id, dst_id, kind)
            );
            CREATE INDEX IF NOT EXISTS idx_sem_edge_dst ON sem_edge (dst_id);
            COMMIT;
            "#,
        )?;
        crate::db_migrate::additive_schema(&conn);
        // v6: flat v4/v5 `tab_layout` → worktree groups (idempotent).
        migrate_tab_layout_v6(&conn);
        crate::host_db::migrate_v30(&conn)?;
        crate::db_placement::migrate_v34(&conn)?;
        crate::host_db::migrate_v35(&conn);
        crate::db_compute::migrate_v36(&conn)?;
        crate::db_iroh::migrate_v38(&conn)?;
        crate::db_control::migrate_v40(&conn)?;
        Ok(Db {
            conn,
            schema_mismatch,
        })
    }

    pub(crate) fn map_share_row(r: &rusqlite::Row) -> rusqlite::Result<ShareRow> {
        Ok(ShareRow {
            worktree: r.get(0)?,
            local_port: r.get::<_, i64>(1)? as u16,
            provider: r.get(2)?,
            public_url: r.get(3)?,
            state: r.get(4)?,
            created_at: r.get(5)?,
        })
    }

    pub(crate) fn map_forward_row(r: &rusqlite::Row) -> rusqlite::Result<ForwardRow> {
        Ok(ForwardRow {
            worktree: r.get(0)?,
            container_port: r.get::<_, i64>(1)? as u16,
            host_port: r.get::<_, i64>(2)? as u16,
            url: r.get(3)?,
            created_at: r.get(4)?,
        })
    }

    // --- notifications inbox -------------------------------------------------

    pub(crate) fn notifications_query(
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

    /// Shared implementation: unread (`read=0`) notifications with a non-empty
    /// worktree, grouped by worktree, where `kind` is one of `kinds`. Builds a
    /// `kind IN (?, …)` clause so a config priority remap reclassifies counts
    /// without touching stored rows.
    pub(crate) fn unread_counts_for_kinds(
        &self,
        kinds: &[&str],
    ) -> Result<std::collections::BTreeMap<String, usize>> {
        let mut counts = std::collections::BTreeMap::new();
        if kinds.is_empty() {
            return Ok(counts);
        }
        let placeholders = std::iter::repeat_n("?", kinds.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT worktree_path, COUNT(*) FROM notifications \
             WHERE read=0 AND worktree_path != '' AND kind IN ({placeholders}) \
             GROUP BY worktree_path"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(kinds.iter()), |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        })?;
        for row in rows.filter_map(|r| r.ok()) {
            counts.insert(row.0, row.1 as usize);
        }
        Ok(counts)
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
}

pub(crate) use crate::db_migrate::migrate_tab_layout_v6;
#[cfg(test)]
pub(crate) use crate::db_migrate::split_page_suffix;

#[cfg(test)]
#[path = "db_tests.rs"]
mod tests;
