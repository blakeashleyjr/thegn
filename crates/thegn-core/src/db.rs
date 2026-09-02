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
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};

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
/// v13: added the LLM-proxy state tables (removed with the AI layer before the
/// public alpha — pre-existing `proxy_*` tables are simply ignored, never
/// dropped, per the never-reset-user-data migration contract). Also formalizes
/// the already-present `container_events` / `layouts` tables under this
/// version.
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
/// v45: `issue_cache`/`issue_projects` re-keyed `(repo_root, provider)` →
/// `(repo_root, provider, account)` for multiple named accounts per provider.
/// Pure caches → drop + recreate (the background refresh repopulates).
/// v46: one-time read-flag cleanup of the spurious `process_failed` notification
/// pile (no schema change) — see the post-batch migration in `open_at`.
/// v48: adds `worktree_titles` (the last OSC window title per worktree, so the
/// sidebar keeps a worktree's dynamic title across switches, parking, cold
/// resurrects, and restarts — seeded at startup, refreshed live).
/// v50: re-keys `attention_acks` to `(worktree_path, reason)` and adds an
/// `episode` column, so several needs-you signals on one worktree can be acked
/// independently and a cache-derived signal (CI run, PR head commit) has a
/// restart-durable episode identity instead of a bare null `since`.
/// v51: adds `pr_queue` (the PR queue — queued pull requests on a forge, their
/// blocker, agent budget, and last observed head; see [`crate::pr_queue`]).
/// Keyed by `<repo_root>#<number>` rather than a worktree path, because a queued
/// PR need not have a local checkout.
/// v52: adds `calendar_events` (per-account event cache, one row per event so
/// an incremental sync can apply tombstones without a full refetch) and
/// `calendar_sync` (per-account cursor + last-fetch bookkeeping). Both are pure
/// caches — the provider is the source of truth — so they are always safe to
/// drop and let the next sync repopulate.
/// v53: adds `usage_samples` (one row per AI-account rate-limit window per poll)
/// — the history behind the usage sparkline and the reset-window forecast. A
/// pure cache: the provider is the source of truth, so it is always safe to drop
/// and let the next poll repopulate. Pruned to `[usage] history_days`.
/// v54: adds `projects` + `workspaces.project_id` ([`crate::store::ProjectStore`]) —
/// a grouping layer above workspaces (the zones *shape*, zero policy). Membership
/// is a nullable `workspaces.project_id` (NULL = unprojected); exclusive by
/// construction. No cross-repo feature link rows are stored (feature sets are
/// derived from git). DDL in `db_migrate`.
///
/// v55: adds `model_proxy_requests` (per-request metadata audit rows for the
/// resurrected model proxy — timings, tokens incl. cache-read/creation, cost,
/// caller scope; never any message content) and `model_proxy_budget_state`
/// (per-scope rolling-window spend accumulators). Both are fresh names — the
/// orphaned pre-alpha `proxy_*` tables are never reused, migrated, or dropped.
///
/// v56: adds four nullable columns to `agent_dispatches` — `stage`,
/// `parent_id`, `session_id`, `artifact_path` — so one roster row can say which
/// pipeline stage it is, which row it was chunked out of, which daemon session
/// runs it, and where its handoff artifact lives. Purely additive idempotent
/// `ALTER`s in `db_migrate::additive_schema`; every pre-v56 row reads
/// back `NULL` (⇒ `None`), which is exactly the pre-change behaviour. The
/// roster gains columns, never transitions: no thegn code path advances a
/// `stage`.
///
/// v57: adds `session_attention` — the live "raised hand" state, one row per
/// daemon session, deleted the moment the user answers. An `OSC 9` /
/// `OSC 777;notify` signal is state, not an event, so it no longer appends one
/// `agent_attention` inbox row per agent turn (THE-68); the same bump marks the
/// unread rows of that old pile read, once. Pure cache: losing the table costs
/// one stale-free hydration.
///
/// v58: no DDL — a one-time data fix. `agent_dispatches.dispatched_at_ms` rows
/// written while `put_agent_dispatch` stored `util::now()` hold SECONDS in a
/// column every reader treats as milliseconds, so they render as ~20 671 days
/// old. The write side was fixed; those rows never were. This bump multiplies
/// them by 1000. See [`crate::issue::normalize_dispatch_ms`], the read-side
/// guard for values that never pass through this migration.
///
/// v67: adds the bounded CI log cache and autofix handoff dedupe table.
/// Purely additive; CI remains a best-effort cache and the provider is the
/// source of truth.
///
/// v61: adds `agent_dispatches.report` (the worker's structured handoff
/// summary, ≤16 KiB) and the `agent_dispatch_notes` table (per-row progress
/// queue; kept separate from `agent_dispatches.note` which is the daemon's
/// transport-retry observer ledger). Purely additive — a pre-v61 row reads
/// back `report = None`, which is exactly the pre-change behaviour.
///
/// v62: adds `session_forks`, a credential-free lineage cache. Live fork
/// recipes remain daemon memory only; the cache cannot resurrect a process.
///
/// v63: adds `pr_review_cache`, one complete PR review snapshot per canonical
/// worktree key. It is a best-effort cache; the branch, PR number, and head OID
/// are retained beside the JSON so stale feedback cannot silently attach to a
/// different PR.
///
/// v64: adds trusted automation throttle/override state and a bounded audit
/// log (THE-21). Both are cache/audit data; action truth remains in catalog
/// providers.
///
/// v65: adds nullable review-task identity/revision/prompt metadata and durable
/// forge-action retry bookkeeping to `agent_dispatches`, and freezes active
/// review-task inputs so one newer snapshot is retained on the same row until
/// the active handoff can safely finish or promote it (THE-22). A partial
/// unique index makes `(task_kind, source_key)` one durable task even under
/// concurrent refreshes.
///
/// THE-21 and THE-22 both originally claimed v64 while in flight; THE-22 takes
/// 65 because its columns are additive on top of THE-21's tables. A single
/// racing integer is the wrong allocation mechanism — see the pipeline
/// follow-ups.
///
/// This build also adds, as guarded idempotent ALTERs rather than a version of
/// their own, `agent_dispatches.exit_code` / `.exited_at_ms` and the
/// `pipeline_leases` table: the worker's recorded exit (so a row still
/// `running` because nobody closed it is distinguishable from one whose worker
/// is alive — the 2026-08-29 conflation) and monitor ownership.
///
/// v66 adds `autopilot_runs`, the provider-qualified issue claim/correlation
/// journal. It is additive and never replaces the existing dispatch roster or
/// issue cache.
///
/// v67: adds the bounded CI log cache and autofix handoff dedupe table.
/// Purely additive; CI remains a best-effort cache and the provider is
/// the source of truth.
pub const SCHEMA_VERSION: i64 = 67;

/// Escape hatch for [`schema_refusal`] — set to `1`/`true` to run a build older
/// than the on-disk schema anyway (read-only, as before). Deliberately awkward:
/// the tolerant open is a debugging affordance now, not a default.
pub const ALLOW_OLD_BUILD_ENV: &str = "THEGN_ALLOW_SCHEMA_DOWNGRADE";

/// Whether this build must refuse to operate a database at `on_disk`.
///
/// # Why refusing beats tolerating
///
/// The schema is additive, so an older build's *named-column* reads still
/// work — which is what made tolerance look safe. What it cannot do is see
/// columns and tables the newer build writes, and the pipeline roster is
/// exactly that kind of state: on 2026-08-29 a v57 daemon drove a v62 roster
/// for hours, could not see the `report` column the newer build gated
/// completion on, and re-dispatched work it believed unfinished — while
/// emitting 326,912 identical mismatch warnings. A build that cannot see the
/// state it is deciding on must not decide.
///
/// Pure, so the policy is unit-testable without a database or an environment.
pub fn schema_refusal(on_disk: i64, build: i64, allow_override: bool) -> Option<String> {
    if on_disk <= build || allow_override {
        return None;
    }
    Some(format!(
        "database schema v{on_disk} was written by a newer thegn than this build (v{build}).\n\
         Refusing to run: this build cannot see the columns the newer one writes, so it would \
         mis-read live state (a pipeline roster, a merge queue) and act on the gap.\n\
         Fix: rebuild or reinstall thegn so the binary matches the database \
         (`just build` in a checkout, `nix profile upgrade` for an installed copy), and make \
         sure the daemon is restarted from the same build as the CLI \
         (`thegn doctor` prints both).\n\
         Override (read-only, for debugging only): {ALLOW_OLD_BUILD_ENV}=1"
    ))
}

/// The state DB's on-disk `user_version`, read through a read-only connection.
///
/// Deliberately independent of [`Db::open`]: when the schema is NEWER than this
/// build, `open` refuses — and that is exactly the moment `thegn doctor` most
/// needs to print the two numbers. `None` when there is no database yet or it
/// cannot be read at all.
pub fn on_disk_schema_version() -> Option<i64> {
    let path = db_path();
    if !path.exists() {
        return None;
    }
    let conn =
        Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).ok()?;
    conn.query_row("PRAGMA user_version", [], |r| r.get(0)).ok()
}

/// Read the [`ALLOW_OLD_BUILD_ENV`] escape hatch.
fn schema_downgrade_allowed() -> bool {
    matches!(
        std::env::var(ALLOW_OLD_BUILD_ENV).as_deref(),
        Ok("1") | Ok("true") | Ok("yes")
    )
}

/// The process kind presented to `[database].migration_authority`.
///
/// A controller is a long-lived owner of the shared state (the interactive
/// compositor, bare pane daemon, or `serve`). Everything else is a client — in
/// particular, worker-side CLI commands found through a worktree's `PATH`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MigrationActor {
    Controller,
    Client,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MigrationPolicy {
    authority: crate::config::MigrationAuthority,
    actor: MigrationActor,
    pinned_executable: Option<PathBuf>,
    current_executable: Option<PathBuf>,
}

struct MigrationLease {
    database: PathBuf,
    file: std::fs::File,
}

struct MigrationRuntime {
    policy: MigrationPolicy,
    /// Once this process has opened the shared DB, keep a shared schema lease
    /// for its lifetime. A migrator needs the exclusive side of the same lock,
    /// so a rebuilt controller cannot advance the schema underneath an older
    /// controller that is still making decisions from it.
    lease: Mutex<Option<MigrationLease>>,
}

static MIGRATION_RUNTIME: OnceLock<MigrationRuntime> = OnceLock::new();

fn canonical_executable(path: &str) -> Result<PathBuf> {
    let expanded = PathBuf::from(util::expand_tilde(path.trim()));
    if !expanded.is_absolute() {
        anyhow::bail!(
            "[database] migration_executable must be an absolute path (got {})",
            expanded.display()
        );
    }
    std::fs::canonicalize(&expanded).map_err(|e| {
        anyhow::anyhow!(
            "cannot resolve [database] migration_executable {}: {e}",
            expanded.display()
        )
    })
}

/// Install the startup-only migration policy before the first shared DB open.
///
/// Re-installing the identical policy is harmless (`open` can fall through to
/// the compositor after first trying delivery). A different second install is
/// refused: changing schema authority live would make the process-lifetime
/// lease ambiguous.
pub fn install_migration_policy(
    cfg: &crate::config::DatabaseConfig,
    actor: MigrationActor,
) -> Result<()> {
    let pinned_executable = if cfg.migration_executable.trim().is_empty() {
        None
    } else {
        Some(canonical_executable(&cfg.migration_executable)?)
    };
    // `self_exe_path`, not `current_exe`: once the binary is rebuilt in place
    // the raw value carries Linux's `" (deleted)"` marker, `canonicalize` then
    // fails, and this drops to `None` — which never equals `pinned_executable`,
    // so the configured controller silently stops being recognized as one and
    // migrations are refused with a message about the wrong executable.
    let current_executable =
        crate::util::self_exe_path().and_then(|p| std::fs::canonicalize(p).ok());
    let policy = MigrationPolicy {
        authority: cfg.migration_authority,
        actor,
        pinned_executable,
        current_executable,
    };
    if let Some(existing) = MIGRATION_RUNTIME.get() {
        if existing.policy == policy {
            return Ok(());
        }
        anyhow::bail!(
            "database migration policy was already installed for this process and cannot be changed live"
        );
    }
    MIGRATION_RUNTIME
        .set(MigrationRuntime {
            policy,
            lease: Mutex::new(None),
        })
        .map_err(|_| anyhow::anyhow!("database migration policy was installed concurrently"))
}

/// Pure authority decision, split out so all policy modes are exhaustively
/// testable without touching a process-global config or a file lock.
pub fn migration_refusal(
    authority: crate::config::MigrationAuthority,
    actor: MigrationActor,
    executable_matches: bool,
    observed: i64,
) -> Option<&'static str> {
    // Creating a database is not advancing one. At `user_version == 0` there is
    // nothing on disk for another process to be holding a stale view of, and —
    // decisively — there is no controller yet either: the whole point of the
    // policy is to elect one, so applying it to bootstrap makes a fresh install
    // unusable by every ordinary CLI, and every isolated test that spawns one.
    // Authority governs UPGRADES; `disabled` still means disabled.
    if observed == 0 && authority != crate::config::MigrationAuthority::Disabled {
        return None;
    }
    if !executable_matches {
        return Some("this executable is not the configured migration executable");
    }
    match authority {
        crate::config::MigrationAuthority::Any => None,
        crate::config::MigrationAuthority::Controller if actor == MigrationActor::Controller => {
            None
        }
        crate::config::MigrationAuthority::Controller => {
            Some("ordinary CLI processes are not database migration controllers")
        }
        crate::config::MigrationAuthority::Disabled => {
            Some("automatic database migrations are disabled")
        }
    }
}

fn schema_lock_path(database: &Path) -> PathBuf {
    let mut path = database.as_os_str().to_os_string();
    path.push(".schema.lock");
    PathBuf::from(path)
}

fn open_schema_lock(database: &Path) -> Result<std::fs::File> {
    let path = schema_lock_path(database);
    std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .map_err(|e| anyhow::anyhow!("open schema lease {}: {e}", path.display()))
}

fn try_shared_lock(file: &std::fs::File, database: &Path) -> Result<()> {
    match file.try_lock_shared() {
        Ok(()) => Ok(()),
        Err(std::fs::TryLockError::WouldBlock) => anyhow::bail!(
            "database schema migration is currently in progress for {}; retry shortly",
            database.display()
        ),
        Err(std::fs::TryLockError::Error(e)) => {
            anyhow::bail!("take shared schema lease for {}: {e}", database.display())
        }
    }
}

fn try_exclusive_lock(file: &std::fs::File, database: &Path) -> Result<()> {
    match file.try_lock() {
        Ok(()) => Ok(()),
        Err(std::fs::TryLockError::WouldBlock) => anyhow::bail!(
            "refusing database schema migration for {}: another thegn process is still using the old schema. Stop/restart the running host and daemon after rebuilding, then retry",
            database.display()
        ),
        Err(std::fs::TryLockError::Error(e)) => {
            anyhow::bail!(
                "take exclusive schema lease for {}: {e}",
                database.display()
            )
        }
    }
}

fn query_user_version(conn: &Connection) -> i64 {
    conn.query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap_or(0)
}

/// Holds both the in-process lease slot and the cross-process exclusive lock
/// throughout the entire migration batch. A successful migration calls
/// [`MigrationGuard::finish`] to join the lifetime shared lease and re-check the
/// version before the new DB handle can escape. An error simply drops/unlocks
/// the guard; no partially initialized `Db` is returned.
struct MigrationGuard {
    slot: MutexGuard<'static, Option<MigrationLease>>,
    database: PathBuf,
    file: Option<std::fs::File>,
}

impl MigrationGuard {
    fn finish(mut self, conn: &Connection) -> Result<()> {
        let file = self
            .file
            .take()
            .ok_or_else(|| anyhow::anyhow!("database migration lease disappeared"))?;
        file.unlock()
            .map_err(|e| anyhow::anyhow!("release exclusive schema lease: {e}"))?;
        // Do not leave a check/use gap after the final version stamp. If a
        // still-newer authorized controller wins the unlock race, wait for it,
        // then observe its version below and refuse this now-older process.
        file.lock_shared()
            .map_err(|e| anyhow::anyhow!("join shared schema lease: {e}"))?;
        let observed = query_user_version(conn);
        if let Some(msg) = schema_refusal(observed, SCHEMA_VERSION, false) {
            let _ = file.unlock();
            anyhow::bail!(msg);
        }
        *self.slot = Some(MigrationLease {
            database: self.database.clone(),
            file,
        });
        Ok(())
    }
}

impl Drop for MigrationGuard {
    fn drop(&mut self) {
        let Some(file) = self.file.take() else {
            return;
        };
        let _ = file.unlock();
    }
}

struct SchemaAccess {
    version: i64,
    _migration: Option<MigrationGuard>,
}

/// Join the process-lifetime shared schema lease, or (only when authorized)
/// take its exclusive side for a pending migration. Re-read `user_version`
/// after the lock is held to close the check/lock race.
fn prepare_schema_access(conn: &Connection, database: &Path, initial: i64) -> Result<SchemaAccess> {
    let Some(runtime) = MIGRATION_RUNTIME.get() else {
        // Library/tests that do not install a production policy retain the
        // explicit-path API's historical behavior.
        return Ok(SchemaAccess {
            version: initial,
            _migration: None,
        });
    };
    let mut slot = runtime
        .lease
        .lock()
        .map_err(|_| anyhow::anyhow!("database schema lease was poisoned"))?;

    if let Some(lease) = slot.as_ref()
        && lease.database == database
    {
        return Ok(SchemaAccess {
            version: query_user_version(conn),
            _migration: None,
        });
    }
    if let Some(lease) = slot.take() {
        let _ = lease.file.unlock();
    }

    let mut observed = initial;
    if observed >= SCHEMA_VERSION {
        let file = open_schema_lock(database)?;
        try_shared_lock(&file, database)?;
        observed = query_user_version(conn);
        if observed >= SCHEMA_VERSION {
            *slot = Some(MigrationLease {
                database: database.to_path_buf(),
                file,
            });
            return Ok(SchemaAccess {
                version: observed,
                _migration: None,
            });
        }
        // A migration/downgrade landed between the first version read and our
        // shared lock. Release and make the authority decision under exclusive.
        let _ = file.unlock();
    }

    let executable_matches = match &runtime.policy.pinned_executable {
        None => true,
        Some(pin) => runtime.policy.current_executable.as_ref() == Some(pin),
    };
    if let Some(reason) = migration_refusal(
        runtime.policy.authority,
        runtime.policy.actor,
        executable_matches,
        observed,
    ) {
        let pin = runtime
            .policy
            .pinned_executable
            .as_ref()
            .map(|p| format!(" (configured executable: {})", p.display()))
            .unwrap_or_default();
        anyhow::bail!(
            "refusing to migrate database schema v{observed} to v{SCHEMA_VERSION}: {reason}{pin}. Launch the configured controller after rebuilding it"
        );
    }

    let file = open_schema_lock(database)?;
    try_exclusive_lock(&file, database)?;
    observed = query_user_version(conn);
    if observed >= SCHEMA_VERSION {
        // Another authorized controller completed the migration before this
        // process won the lock. Join as a normal lifetime reader.
        let _ = file.unlock();
        try_shared_lock(&file, database)?;
        *slot = Some(MigrationLease {
            database: database.to_path_buf(),
            file,
        });
        return Ok(SchemaAccess {
            version: observed,
            _migration: None,
        });
    }

    Ok(SchemaAccess {
        version: observed,
        _migration: Some(MigrationGuard {
            slot,
            database: database.to_path_buf(),
            file: Some(file),
        }),
    })
}
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
    /// The worktree's `location` descriptor (mirrored from `worktrees.location`
    /// at enqueue time): empty/`local` for an on-host worktree, or a small JSON
    /// ssh/provider blob. Lets the queue attribute a row to a host without a
    /// live git shell — the cross-host drain reads it to decide whether the
    /// branch tip must be fetched into the target store. See [`crate::remote`].
    #[serde(default)]
    pub location: String,
    /// How many agent-dispatch → re-fold cycles have been spent on this row.
    /// Persisted (v49) so `agent_max_attempts` is a budget for the branch, not
    /// for one invocation of the drain.
    #[serde(default)]
    pub agent_attempts: u32,
}

/// One `pr_queue` row — a pull request thegn is shepherding on a forge.
///
/// Keyed by repo + number rather than worktree, because a queued PR need not
/// have a local checkout. `status` is a [`crate::pr_queue::PrqStatus`] word and
/// `blocker` a [`crate::pr_queue::Blocker`] word.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct PrQueueRow {
    /// `<repo_root>#<number>` — the primary key.
    pub key: String,
    pub repo_root: String,
    pub number: u64,
    /// The worktree this entry was queued from. `None` for a PR with no local
    /// checkout — the driver reports that as needing a human rather than
    /// silently skipping the row, since an agent has nowhere to work.
    pub worktree: Option<String>,
    pub branch: String,
    pub base_branch: String,
    /// Which forge owns it (`github` today; the seam is a trait).
    pub forge: String,
    pub status: String,
    /// The classified blocker word, or `None` before the first refresh.
    pub blocker: Option<String>,
    /// Human-readable detail for the current status (failing checks, an error).
    pub detail: Option<String>,
    pub agent_attempts: u32,
    /// The head commit thegn last observed. A change it did not cause means a
    /// teammate pushed — see [`crate::pr_queue::foreign_push`].
    pub last_head_oid: Option<String>,
    pub queued_at: i64,
    pub updated_at: i64,
}

impl PrQueueRow {
    /// The primary key for a repo + PR number.
    pub fn make_key(repo_root: &str, number: u64) -> String {
        format!("{repo_root}#{number}")
    }
}

// Share/forward resurrection rows live in `models` (size-capped file); the
// `crate::db::{ShareRow, ForwardRow}` paths stay valid via this re-export.
pub use crate::models::{ForwardRow, ShareRow};

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

/// How [`Db::init`] treats a connection, decided from the on-disk
/// `user_version`. `Fast` (on-disk >= [`SCHEMA_VERSION`]) skips the
/// schema batch, migrations, and startup prunes entirely — safe because
/// the version is stamped only after a full init completes *and* the
/// schema is purely additive (`IF NOT EXISTS` DDL + idempotent ALTER
/// probes), so an older binary's named-column reads/writes are unaffected
/// by columns it doesn't know. Only `on_disk < current` (fresh or genuinely
/// stale) takes `Full` — a migration is due only then.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OpenMode {
    Fast,
    Full,
}

/// Pure decision for the open fast path (unit-tested exhaustively).
/// `on_disk >= current` is Fast: the schema is additive by construction, so
/// an older binary's named-column reads/writes are unaffected by columns it
/// doesn't know. Only `on_disk < current` (fresh or genuinely stale) takes
/// Full — a migration is due then.
pub(crate) fn open_mode(on_disk: i64, current: i64) -> OpenMode {
    if on_disk >= current {
        OpenMode::Fast
    } else {
        OpenMode::Full
    }
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
            // Owner-only (0700) on the state dir + 0600 on the DB file below:
            // thegn.db holds some live bearer credentials (iroh pair secrets)
            // sent verbatim, plus per-repo history — it must not be
            // world-readable under a lax umask. Best-effort, matching the
            // secret-file writes elsewhere (sandbox/vpn/share). (THE-66 moved the
            // Kaneo device-flow token OUT of the DB into the broker; the
            // `kaneo_auth` row now holds only a `file:`/`env:` SecretRef.)
            let _ = crate::fsperm::restrict_dir_to_owner(dir); // best-effort: hardening: a failed chmod must never block DB open
        }
        let db = Self::init_shared(Self::open_connection(&path)?, &path)?;
        let _ = crate::fsperm::restrict_to_owner(&path); // best-effort: hardening: a failed chmod must never block DB open
        // The common fast-path init (user_version already current) skips the
        // startup prunes so a plain open takes NO write lock. Run them once
        // per process here so a long-lived install still gets its growth
        // bound. (A full init also prunes inline; the overlap on the first
        // open after a migration is an idempotent no-op.)
        static PRUNE_ONCE: std::sync::Once = std::sync::Once::new();
        if db.schema_mismatch.is_none() {
            PRUNE_ONCE.call_once(|| db.startup_prune());
        }
        Ok(db)
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
            let _ = crate::fsperm::restrict_dir_to_owner(dir);
        }
        if path == db_path() {
            let db = Self::init_shared(Self::open_connection(path)?, path)?;
            let _ = crate::fsperm::restrict_to_owner(path); // best-effort: hardening: a failed chmod must never block DB open
            Ok(db)
        } else {
            let db = Self::init(Self::open_connection(path)?)?;
            let _ = crate::fsperm::restrict_to_owner(path); // best-effort: hardening: a failed chmod must never block DB open
            Ok(db)
        }
    }

    /// Open an existing DB read-only while participating in its WAL locking.
    ///
    /// Unlike [`Self::open_read_only_at`], this may use existing WAL/SHM
    /// sidecars so a real migration preflight sees the latest target rows. It
    /// still performs no schema initialization, migration, or startup prune.
    pub fn open_read_only_wal_at(path: &std::path::Path) -> Result<Option<Db>> {
        if !path.is_file() {
            return Ok(None);
        }
        let conn = Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.busy_timeout(std::time::Duration::from_millis(5000))?;
        let ver: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap_or(0);
        Ok(Some(Db {
            conn,
            schema_mismatch: crate::db_migrate::detect_newer_schema(ver, SCHEMA_VERSION),
        }))
    }

    /// Open an existing state DB without creating, migrating, pruning, or
    /// changing its journal mode. This is the only safe opener for commands
    /// whose dry-run contract is strictly read-only. An absent file is
    /// represented as `None` so callers can inspect an empty, not-yet-created
    /// target without manufacturing a database.
    pub fn open_read_only_at(path: &std::path::Path) -> Result<Option<Db>> {
        if !path.is_file() {
            return Ok(None);
        }
        // A plain read-only open of a WAL-mode database is still allowed to
        // create `-wal`/`-shm` sidecars. `immutable=1` is SQLite's explicit
        // no-write/no-lock URI mode, which is required by dry-run callers.
        let uri = util::immutable_sqlite_uri(&path.canonicalize()?);
        let conn = Connection::open_with_flags(
            uri,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
                | rusqlite::OpenFlags::SQLITE_OPEN_URI
                | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.busy_timeout(std::time::Duration::from_millis(5000))?;
        let ver: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap_or(0);
        Ok(Some(Db {
            conn,
            schema_mismatch: crate::db_migrate::detect_newer_schema(ver, SCHEMA_VERSION),
        }))
    }

    /// Read-only counterpart to [`Db::open`] for dry-run command paths.
    pub fn open_read_only() -> Result<Option<Db>> {
        Self::open_read_only_at(&db_path())
    }

    /// Open a state DB read-only when it was written by a newer build. The
    /// initial connection is only used to read `user_version`; reopening with
    /// read-only flags prevents callers from mutating columns this build does
    /// not understand after the tolerant open succeeds.
    fn open_connection(path: &std::path::Path) -> Result<Connection> {
        let conn = Connection::open(path)?;
        let ver: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap_or(0);
        if ver > SCHEMA_VERSION {
            drop(conn);
            return Ok(Connection::open_with_flags(
                path,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            )?);
        }
        Ok(conn)
    }

    /// Apply pragmas, migration, and schema to a fresh connection.
    fn init(conn: Connection) -> Result<Db> {
        Self::init_with(conn, schema_downgrade_allowed(), None)
    }

    fn init_shared(conn: Connection, path: &Path) -> Result<Db> {
        Self::init_with(conn, schema_downgrade_allowed(), Some(path))
    }

    /// Open at an explicit path, tolerating a newer on-disk schema (read-only)
    /// instead of refusing it — the programmatic form of the
    /// [`ALLOW_OLD_BUILD_ENV`] escape hatch, so the tolerant branch is
    /// reachable (and testable) without mutating process-global environment.
    pub fn open_at_allowing_older_build(path: &std::path::Path) -> Result<Db> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        Self::init_with(Self::open_connection(path)?, true, None)
    }

    fn init_with(
        conn: Connection,
        allow_downgrade: bool,
        shared_path: Option<&Path>,
    ) -> Result<Db> {
        conn.busy_timeout(std::time::Duration::from_millis(5000))?;
        let initial_ver = query_user_version(&conn);
        // The returned exclusive guard (when any) stays in scope until the
        // final `user_version` stamp and every gated migration are complete.
        // Fast/current opens retain a process-lifetime shared lease instead.
        let schema_access = if let Some(path) = shared_path {
            prepare_schema_access(&conn, path, initial_ver)?
        } else {
            SchemaAccess {
                version: initial_ver,
                _migration: None,
            }
        };
        let ver = schema_access.version;
        let mut migration_guard = schema_access._migration;

        // A newer DB is opened read-only. In particular, do not change its
        // journal mode or run the startup prune: those are writes even though
        // the schema/migration ladder is being skipped.
        if ver > SCHEMA_VERSION {
            // One actionable error, not a warning repeated per open. The old
            // behaviour (warn + carry on read-only) is what let a v57 runtime
            // drive a v62 roster; it survives only behind the explicit
            // `ALLOW_OLD_BUILD_ENV` override, which still warns exactly once.
            if let Some(msg) = schema_refusal(ver, SCHEMA_VERSION, allow_downgrade) {
                anyhow::bail!(msg);
            }
            static MISMATCH_WARNED: std::sync::Once = std::sync::Once::new();
            MISMATCH_WARNED.call_once(|| {
                tracing::warn!(
                    target: "thegn::db",
                    on_disk = ver,
                    build = SCHEMA_VERSION,
                    override_env = ALLOW_OLD_BUILD_ENV,
                    "database schema v{ver} is newer than this build (v{SCHEMA_VERSION}); \
                     running anyway because {ALLOW_OLD_BUILD_ENV} is set — data written by \
                     the newer build is invisible to this one"
                );
            });
            return Ok(Db {
                conn,
                schema_mismatch: Some(ver),
            });
        }

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
        // Fast path: `user_version` is stamped at the END of a full init
        // (see below), so `on_disk >= current` PROVES the whole schema batch +
        // every migration completed — the ALTER probes, the DDL transaction,
        // the migrate_vNN passes, and the prunes are all no-ops that would
        // still take the WAL write lock. Skip them. A newer-schema DB
        // (different branch sharing this file) is tolerated: the schema is
        // purely additive, so reads/writes on named columns are unaffected.
        // Only `on_disk < current` (fresh or genuinely stale) takes the full
        // path with its migrations.
        if open_mode(ver, SCHEMA_VERSION) == OpenMode::Fast {
            return Ok(Db {
                conn,
                schema_mismatch: None,
            });
        }
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
            let _ = conn.execute("ALTER TABLE repos ADD COLUMN session_name TEXT", []); // best-effort: idempotent additive migration: the ignore is the already-applied no-op
        }
        // v28: `my_work_cache` re-keyed from a single `id=0` row to per-`scope`
        // rows. It's a pure cache (rebuilt by the background worker), so drop the
        // old-shape table here; the CREATE below recreates it with the new shape
        // and the next refresh repopulates it.
        if ver < 28 {
            let _ = conn.execute("DROP TABLE IF EXISTS my_work_cache", []); // best-effort: idempotent additive migration: the ignore is the already-applied no-op
        }
        // v45: `issue_cache`/`issue_projects` gain an `account` PK column so
        // multiple accounts per provider don't clobber each other. Pure caches
        // (rebuilt by the background refresh), so drop the old-shape tables here;
        // the CREATE below recreates them with the new PK and the next refresh
        // repopulates. `account=''` is the legacy/synthesized-account sentinel.
        if ver < 45 {
            // best-effort: idempotent additive migration: the ignore is the already-applied no-op
            let _ = conn.execute_batch(
                "DROP TABLE IF EXISTS issue_cache;
                 DROP TABLE IF EXISTS issue_projects;",
            );
        }
        // NB: user_version is stamped at the END of init (not here), AFTER the
        // schema batch + every `ver`-gated post-batch cleanup, so a crash mid-init
        // leaves the OLD version on disk and the next open re-runs the (idempotent)
        // gated steps rather than skipping a migration that never actually ran.
        // A newer-schema DB (different branch sharing this file): warn + tolerate.
        let schema_mismatch = crate::db_migrate::detect_newer_schema(ver, SCHEMA_VERSION);

        let _ = conn.execute("ALTER TABLE worktrees ADD COLUMN sandbox_backend TEXT", []); // best-effort: idempotent additive migration: the ignore is the already-applied no-op
        // v31: per-language LOC report JSON alongside the total (idempotent).
        let _ = conn.execute("ALTER TABLE loc_cache ADD COLUMN report_json TEXT", []); // best-effort: idempotent additive migration: the ignore is the already-applied no-op

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
            -- v63: complete PR review conversation + PR-head diff snapshot.
            -- Identity columns make stale cache validation possible without
            -- parsing the payload and the JSON is replaced atomically.
            CREATE TABLE IF NOT EXISTS pr_review_cache (
              worktree   TEXT PRIMARY KEY,
              branch     TEXT NOT NULL,
              pr_number  INTEGER NOT NULL,
              head_oid   TEXT NOT NULL,
              json       TEXT NOT NULL,
              fetched_at INTEGER NOT NULL
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
            -- v62: bounded, redacted per-job CI log tails. The provider is
            -- authoritative; this table only makes read paths instant and
            -- resilient to a transient provider failure.
            CREATE TABLE IF NOT EXISTS ci_log_cache (
              worktree   TEXT NOT NULL,
              run_id     TEXT NOT NULL,
              job_id     TEXT NOT NULL,
              job_name   TEXT NOT NULL,
              head_sha   TEXT NOT NULL DEFAULT '',
              text       TEXT NOT NULL,
              truncated  INTEGER NOT NULL DEFAULT 0,
              redacted   INTEGER NOT NULL DEFAULT 1,
              fetched_at INTEGER NOT NULL,
              PRIMARY KEY (worktree, run_id, job_id)
            );
            CREATE INDEX IF NOT EXISTS idx_ci_log_cache_worktree
              ON ci_log_cache(worktree, fetched_at);
            -- v62: intent marker for a single `(worktree, run, job, head)`
            -- autofix spend. It is cache-side state, never provider truth.
            CREATE TABLE IF NOT EXISTS ci_autofix_dedupe (
              worktree   TEXT NOT NULL,
              run_id     TEXT NOT NULL,
              job_id     TEXT NOT NULL,
              head_sha   TEXT NOT NULL,
              claimed_at INTEGER NOT NULL,
              PRIMARY KEY (worktree, run_id, job_id, head_sha)
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
            -- Per-worktree tokei report, written by the off-loop LOC scan and
            -- painted from here (see `measure::loc`). `loc` is the pre-v31 total
            -- and is now WRITE-ONLY: every reader goes through `report_json`,
            -- which carries the total alongside the per-language rows. Kept
            -- deliberately rather than migrated away — dropping a column costs a
            -- schema bump for no user-visible gain, and the plain integer keeps
            -- the table legible to `sqlite3` and to any pre-v31 reader.
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
            -- v48: last OSC window title per worktree. The sidebar renders each
            -- worktree's dynamic (process-set) title; it lives only in the live
            -- pane emulator, so without this it vanished when the worktree's
            -- workspace was parked / cold-resurrected / on restart. Seeded into
            -- the sidebar map at startup and refreshed on change.
            CREATE TABLE IF NOT EXISTS worktree_titles (
              worktree   TEXT PRIMARY KEY,
              last_title TEXT,
              updated_at INTEGER
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
            -- v45: `account` (the `[[issue_accounts]]` name, `''` for the
            -- legacy single-account path) joins the PK so multiple accounts of
            -- one provider cache independently.
            CREATE TABLE IF NOT EXISTS issue_cache (
              repo_root  TEXT    NOT NULL,
              provider   TEXT    NOT NULL,
              account    TEXT    NOT NULL DEFAULT '',
              json       TEXT    NOT NULL,
              fetched_at INTEGER NOT NULL,
              PRIMARY KEY (repo_root, provider, account)
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
            -- v45: `account` joins the PK (see `issue_cache`).
            CREATE TABLE IF NOT EXISTS issue_projects (
              repo_root  TEXT    NOT NULL,
              provider   TEXT    NOT NULL,
              account    TEXT    NOT NULL DEFAULT '',
              json       TEXT    NOT NULL,
              fetched_at INTEGER NOT NULL,
              PRIMARY KEY (repo_root, provider, account)
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
            -- Supporting index for the two hot inbox queries: the per-hydration
            -- unread-badge count (`WHERE read=0 AND kind IN(…) GROUP BY
            -- worktree_path`, run twice per hydration) and the newest-first
            -- inbox list. Without it every count/list is a full table scan +
            -- sort, and the table grows monotonically (read rows are marked, not
            -- deleted, and only pruned by `prune_notifications`).
            CREATE INDEX IF NOT EXISTS idx_notifications_unread
              ON notifications (read, kind, worktree_path);
            CREATE INDEX IF NOT EXISTS idx_notifications_created
              ON notifications (created_at_ms DESC);
            -- v41 (re-keyed in v50): acknowledgement of a "Needs you" attention
            -- signal. Stores the exact (reason, since, episode) that was showing
            -- when the user quieted it, so the nag stays silenced for *that
            -- episode* only (a changed reason, advanced `since`, or a new
            -- `episode` re-fires — see `attention::AttentionScore::is_acked_by`).
            --
            -- v50 keys on (worktree_path, reason), not worktree_path alone: a
            -- worktree can carry several needs-you signals at once and `score`
            -- only ever reports the most urgent, so a single-row-per-worktree key
            -- meant acking the winner destroyed the ack for the one it outranked.
            -- `episode` gives cache-derived signals (CI runs, PR checks) an
            -- identity of their own; without it they had only a null `since` and
            -- could not be told apart episode-to-episode.
            --
            -- Purely additive cache; git / live state is truth, so a stale row
            -- just re-nags harmlessly.
            CREATE TABLE IF NOT EXISTS attention_acks (
              worktree_path TEXT    NOT NULL,
              reason        TEXT    NOT NULL,
              since         INTEGER,
              episode       INTEGER NOT NULL DEFAULT 0,
              acked_at      INTEGER NOT NULL DEFAULT 0,
              PRIMARY KEY (worktree_path, reason)
            );
            -- v12: agent dispatch registry.  Each row tracks one AI coding
            -- agent assigned to work on one issue in a dedicated worktree.
            CREATE TABLE IF NOT EXISTS agent_dispatches (
              id               INTEGER PRIMARY KEY AUTOINCREMENT,
              issue_id         TEXT    NOT NULL,
              worktree_path    TEXT    NOT NULL,
              agent_name       TEXT    NOT NULL,
              dispatched_at_ms INTEGER NOT NULL,
              status           TEXT    NOT NULL DEFAULT 'queued',
              report           TEXT,
              task_kind        TEXT,
              source_key       TEXT,
              source_revision  TEXT,
              content_revision TEXT,
              prompt           TEXT,
              expected_head_oid TEXT,
              pending_source_revision TEXT,
              pending_content_revision TEXT,
              pending_prompt TEXT,
              pending_expected_head_oid TEXT,
              pending_role TEXT,
              pending_worktree_path TEXT,
              forge_action_attempts INTEGER NOT NULL DEFAULT 0,
              next_forge_action_at_ms INTEGER
            );
            -- v61: per-row progress queue — a worker or monitor appends short
            -- notes (≤4 KiB), read newest-last by dispatch status.
            -- Kept separate from `agent_dispatches.note` (the daemon's
            -- transport-retry observer ledger): conflating them would make
            -- every progress read re-parse for transport artifacts.
            CREATE TABLE IF NOT EXISTS agent_dispatch_notes (
              id             INTEGER PRIMARY KEY AUTOINCREMENT,
              dispatch_id    INTEGER NOT NULL,
              created_at_ms  INTEGER NOT NULL,
              text           TEXT    NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_dispatch_notes_dispatch
              ON agent_dispatch_notes (dispatch_id, created_at_ms);
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
            -- status (the full vocabulary; MqStatus::parse is the one decoder):
            --   queued | folding | verifying | agent_running
            --   landed | ready
            --   deferred | gate_failed | gate_error | needs_human
            -- gate_failed = the gate RAN and went red (a verdict about the code);
            -- gate_error  = the gate could not RUN (an environment fact). They are
            -- separate because only the former may wake the fixing agent.
            CREATE TABLE IF NOT EXISTS merge_queue (
              worktree       TEXT PRIMARY KEY,
              branch         TEXT NOT NULL,
              target_branch  TEXT NOT NULL,
              status         TEXT NOT NULL DEFAULT 'queued',
              queued_at      INTEGER NOT NULL,
              updated_at     INTEGER NOT NULL,
              result_oid     TEXT,
              conflict_paths TEXT,
              error_detail   TEXT,
              -- v44: the worktree's location (mirrored from worktrees.location at
              -- enqueue) so a cross-host drain can attribute a row to a host and
              -- decide whether to fetch its tip into the target store.
              location       TEXT,
              -- v49: agent-dispatch attempts spent on this row, so the budget
              -- survives the drain that spent it. It used to be a per-call local,
              -- so a `needs_human` row that had already exhausted
              -- `agent_max_attempts` got the full budget again on every drain.
              agent_attempts INTEGER NOT NULL DEFAULT 0
            );
            CREATE INDEX IF NOT EXISTS idx_merge_queue_status
              ON merge_queue (status, queued_at);
            -- pr_queue (v50): the PR queue — pull requests being shepherded on a
            -- forge. Keyed by `<repo_root>#<number>`, NOT by worktree, because a
            -- queued PR need not have a local checkout (`worktree` is nullable).
            -- status (PrqStatus::parse is the one decoder):
            --   watching | blocked_ci | blocked_conflict | blocked_review
            --   agent_running | ready | merging | merged | needs_human | closed
            -- `last_head_oid` is what makes the team-safety rules work: a head
            -- thegn did not produce means someone else pushed, which both refills
            -- the agent budget and (under pause_on_foreign_push) stops the agent
            -- rather than racing a teammate.
            CREATE TABLE IF NOT EXISTS pr_queue (
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
            );
            CREATE INDEX IF NOT EXISTS idx_pr_queue_repo
              ON pr_queue (repo_root, status, queued_at);
            -- usage_samples (v53): AI-account rate-limit history, one row per
            -- (account, window) per poll. Feeds the usage section's trend
            -- sparkline and the "you'll hit the cap at …" forecast. `account_key`
            -- is the account's stable identity (see `usage::AccountUsage::key`),
            -- NOT a credential path, so history survives a home being moved or
            -- re-adopted. A pure cache; safe to drop at any time.
            CREATE TABLE IF NOT EXISTS usage_samples (
              account_key  TEXT NOT NULL,
              window       TEXT NOT NULL,
              used_percent REAL NOT NULL,
              resets_at    INTEGER,
              sampled_at   INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_usage_samples
              ON usage_samples (account_key, window, sampled_at);
            -- The read index above is leading-column-keyed, so the retention
            -- DELETE (which knows only a time) would scan the table without
            -- this one.
            CREATE INDEX IF NOT EXISTS idx_usage_samples_time
              ON usage_samples (sampled_at);
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
            -- v47: device-flow access token per Kaneo instance (`base_url`).
            -- Written by `thegn kaneo login`; the Kaneo issue backend falls back
            -- to it when no `api_key` is configured. A cache/credential store,
            -- not a source of truth — safe to drop (re-login re-populates).
            CREATE TABLE IF NOT EXISTS kaneo_auth (
              base_url   TEXT PRIMARY KEY,
              token      TEXT NOT NULL,
              fetched_at INTEGER NOT NULL
            );
            -- v57: live "raised hand" state, one row per daemon session. An OSC 9 /
            -- OSC 777;notify signal is LIVE STATE — deleted the moment the user answers —
            -- not an inbox event, so it no longer appends one `agent_attention`
            -- notification per agent turn (THE-68). This row is the cross-process channel
            -- from the session actor to the compositor's attention scorer. Pure cache:
            -- reaped on answer, on session end, on daemon boot, on `del_worktree`, and by
            -- the startup age sweep.
            CREATE TABLE IF NOT EXISTS session_attention (
              session       TEXT PRIMARY KEY,
              worktree_path TEXT NOT NULL,
              title         TEXT NOT NULL,
              body          TEXT NOT NULL,
              since         INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_session_attention_wt
              ON session_attention (worktree_path);
            -- v64: trusted automation throttle/override state. JSON columns
            -- hold bounded pure-engine ledgers; losing them only resets
            -- throttles and never disables configured rules.
            CREATE TABLE IF NOT EXISTS automation_state (
              rule_id           TEXT PRIMARY KEY,
              enabled_override  INTEGER,
              last_fired_at     INTEGER,
              recent_fires_json TEXT NOT NULL DEFAULT '[]',
              action_fires_json TEXT NOT NULL DEFAULT '{}',
              once_keys_json    TEXT NOT NULL DEFAULT '[]',
              updated_at        INTEGER NOT NULL
            );
            -- v64: metadata-only action audit. Summaries are bounded by the
            -- runtime; full prompts, event bodies, and secrets never land here.
            CREATE TABLE IF NOT EXISTS automation_runs (
              id             INTEGER PRIMARY KEY AUTOINCREMENT,
              rule_id        TEXT NOT NULL,
              event_id       TEXT NOT NULL,
              event_key      TEXT NOT NULL,
              trigger_kind   TEXT NOT NULL,
              event_summary  TEXT NOT NULL DEFAULT '',
              action_cap     TEXT NOT NULL,
              action_summary TEXT NOT NULL DEFAULT '',
              outcome        TEXT NOT NULL,
              skip_reason    TEXT,
              error          TEXT,
              started_at     INTEGER NOT NULL,
              finished_at    INTEGER
            );
            CREATE INDEX IF NOT EXISTS idx_automation_runs_rule_time
              ON automation_runs (rule_id, started_at DESC, id DESC);
            CREATE INDEX IF NOT EXISTS idx_automation_runs_outcome_time
              ON automation_runs (outcome, started_at DESC);
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
        crate::db_calendar::migrate_v52(&conn)?;
        crate::db_model_proxy::migrate_v54(&conn)?;
        crate::db_migrate::migrate_v62(&conn)?;
        crate::db_migrate::migrate_v63_leases(&conn)?;
        crate::db_migrate::migrate_v64(&conn)?;
        crate::db_migrate::migrate_v66(&conn)?;
        crate::db_migrate::migrate_v67(&conn)?;
        if ver < SCHEMA_VERSION {
            crate::db_migrate::verify_v62_schema(&conn)?;
            crate::db_migrate::verify_v63_schema(&conn)?;
            crate::db_migrate::verify_v64_schema(&conn)?;
            crate::db_migrate::verify_v65_schema(&conn)?;
            crate::db_migrate::verify_v66_schema(&conn)?;
            crate::db_migrate::verify_v67_schema(&conn)?;
        }
        // v46: one-time cleanup of the spurious `process_failed` notification
        // pile that accrued while routine shell teardown (and unreapable /
        // relay-lost `None` exits) were mis-classified as failures — see
        // `event_bus::classify_process_exit`. Mark those unread rows read so the
        // sidebar's ⚠ badge + "process failed" attention hint clear immediately.
        // Gated on the pre-bump on-disk version so it runs exactly once — genuine
        // future task-failure alerts are never touched. Best-effort: the DB is a
        // cache, and a fresh DB simply matches zero rows.
        if ver < 46 {
            let _ = conn.execute(
                "UPDATE notifications SET read=1 WHERE kind='process_failed' AND read=0",
                [],
            );
        }
        // v57: one-time retirement of the `agent_attention` pile that accrued while
        // every OSC raised hand appended an inbox row (one per agent turn, and the
        // row never cleared when the user answered — see THE-68). Those rows are now
        // live state in `session_attention`. Mark the unread ones read so the inbox
        // and the ⚑/✋ counts start clean instead of carrying months of backlog.
        // Gated on the pre-bump on-disk version so it runs exactly once; a deliberate
        // `thegn notify push --urgency alert` raised after the upgrade is untouched.
        // Best-effort: the DB is a cache, and a fresh DB matches zero rows.
        if ver < 57 {
            let _ = conn.execute(
                "UPDATE notifications SET read=1 WHERE kind='agent_attention' AND read=0",
                [],
            );
        }
        // v58: one-time normalization of `agent_dispatches.dispatched_at_ms` rows
        // written while `put_agent_dispatch` stored `util::now()` (SECONDS) into a
        // column every reader treats as milliseconds — a fresh row rendered as ~20 671
        // days old. The write side was fixed; these rows never were. Gated on the
        // pre-bump on-disk version so it runs exactly once, and the predicate is
        // idempotent anyway (a scaled row is above the floor). The literal below is
        // `crate::issue::MS_EPOCH_FLOOR` — SQL cannot bind a Rust const, so the two
        // must be kept in step by hand. Best-effort: the DB is a cache, and a fresh
        // DB matches zero rows.
        if ver < 58 {
            let _ = conn.execute(
                "UPDATE agent_dispatches SET dispatched_at_ms = dispatched_at_ms * 1000 \
                 WHERE dispatched_at_ms > 0 AND dispatched_at_ms < 100000000000",
                [],
            );
        }
        // Stamp the schema version LAST — only now that the whole batch + every
        // `ver`-gated cleanup above has run. A crash before this point leaves the
        // OLD version on disk so the next open re-runs the (idempotent) steps
        // instead of skipping a migration that never completed.
        if ver < SCHEMA_VERSION {
            conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        }
        if let Some(guard) = migration_guard.take() {
            guard.finish(&conn)?;
        }
        let db = Db {
            conn,
            schema_mismatch,
        };
        // Full inits (fresh DB / migration) prune inline; the common fast-path
        // open skips this and relies on the once-per-process prune in `open()`.
        db.startup_prune();
        Ok(db)
    }

    /// Growth bounds for the tables nothing else ever deletes from. Best-effort — the DB
    /// is a cache and this must never gate open.
    ///
    /// - Read notifications are only ever marked (never deleted) by the inbox,
    ///   so without this the table grows monotonically on a long-lived install
    ///   (see `prune_notifications`). Read rows older than 30 days go; unread
    ///   alerts are always kept.
    /// - Attention acks: a table-size sweep, not a semantic — an ack is
    ///   *released* by a new episode (see `attention::ack_expired` for the one
    ///   time-based case), never by this. 90 days is well past any episode's
    ///   useful life.
    /// - Cached calendar events: a subscribed feed accumulates a year of
    ///   history per sync otherwise.
    /// - Session attention: a raised hand is lowered by an answer or by the
    ///   session ending, so a row still up a week later belongs to a session
    ///   that went away without either — a leak, not a demand.
    fn startup_prune(&self) {
        let _ = self.prune_notifications(30 * 24 * 3600); // best-effort: startup prune: growth bound on a disposable table; failure never fails an open
        {
            use crate::store::NotificationStore as _;
            let _ = self.prune_attention_acks(90 * 24 * 3600); // best-effort: startup prune: growth bound on a disposable table; failure never fails an open
        }
        // A raised hand outliving its session by a week is a leak, not a demand.
        {
            use crate::store::NotificationStore as _;
            // best-effort: a growth bound on a disposable cache table, never a
            // reason to fail an open.
            let _ = self.prune_session_attention(7 * 24 * 3600);
        }
        // One-shot events that ended over a year ago go; recurrence masters
        // never do, since an old DTSTART still generates today's occurrences.
        {
            use crate::store::CalendarStore as _;
            let cutoff_ms = (crate::util::now() - 365 * 24 * 3600) * 1000;
            let _ = self.prune_calendar_events(cutoff_ms); // best-effort: startup prune: growth bound on a disposable table; failure never fails an open
        }
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

    /// Delete read (`read=1`) notifications older than `older_than_secs`.
    /// Called on startup to keep the inbox table from growing unbounded — read
    /// rows are otherwise never deleted (only marked), so a long-lived install
    /// accumulates them forever (the v46 `process_failed` pile is the canonical
    /// example: those rows were marked read, not removed). Unread rows are never
    /// pruned regardless of age — an unacknowledged alert must survive. Returns
    /// the number of rows removed. Best-effort: the DB is a cache.
    pub fn prune_notifications(&self, older_than_secs: i64) -> Result<usize> {
        // NB: `created_at_ms` holds unix *seconds* despite the legacy name
        // (see `attention.rs`), and `put_notification` stamps it with
        // `util::now()` (seconds) — so the cutoff is a plain seconds subtraction.
        let cutoff = crate::util::now() - older_than_secs;
        let n = self.conn.execute(
            "DELETE FROM notifications WHERE read=1 AND created_at_ms < ?1",
            rusqlite::params![cutoff],
        )?;
        Ok(n)
    }

    pub(crate) fn notifications_query(
        &self,
        sql: &str,
        limit: usize,
    ) -> Result<Vec<crate::notification::Notification>> {
        // Push the cap into SQL so a bounded inbox open never materializes +
        // sorts the whole (monotonically-growing) table — the Rust-side
        // `out.len() >= limit` break below stays as a belt-and-suspenders guard.
        // `usize::MAX` (the unread feed) means "no cap": leave the SQL untouched.
        let capped;
        let sql = if limit == usize::MAX {
            sql
        } else {
            capped = format!("{sql} LIMIT {limit}");
            &capped
        };
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
