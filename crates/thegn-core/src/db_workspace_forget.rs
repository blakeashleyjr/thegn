//! Offline workspace forgetting: the registry prune behind `thegn repo forget`.
//!
//! The UI's "remove workspace (keep files)" path prunes a workspace's registry
//! rows while the instance is running. Splitting one instance into two needs
//! the same prune on a stopped instance's database copy, plus the rows that
//! drive background work against that workspace's worktrees (queues, pools,
//! hibernation, forwards, shares, automation proposals), so the copy that no
//! longer owns a workspace never acts on it.
//!
//! Everything here is a database operation. Nothing touches files on disk, git
//! worktrees or branches.

use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::{OptionalExtension, params};
use serde::Serialize;

use crate::autopilot::AutopilotState;
use crate::db::Db;
use crate::issue::AgentDispatchStatus;
use crate::store::{
    AutopilotStore, HibernationStore, NotificationStore, PoolStore, WorkspaceStore,
    WorktreeAuxStore,
};

/// Actor recorded on the merge-queue history events this prune writes.
pub const FORGET_ACTOR: &str = "thegn repo forget";

/// Note/reason recorded on abandoned dispatches and stopped autopilot runs.
pub const FORGET_REASON: &str = "workspace forgotten by `thegn repo forget`";

/// One workspace to forget. `slug` keys the persisted tab groups and sidebar
/// state; `None` skips that slug-keyed cleanup (see [`Db::forget_slug`]).
#[derive(Debug, Clone, Copy)]
pub struct ForgetWorkspace<'a> {
    pub repo_path: &'a str,
    pub slug: Option<&'a str>,
    /// The `ui_state` scope holding the sidebar's `pin:` / `collapse:` keys.
    pub sidebar_scope: &'a str,
}

/// What forgetting one workspace removes (dry run) or removed (apply).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct WorkspaceForgetCounts {
    /// `worktrees` registry rows.
    pub worktrees: usize,
    /// Persisted tab groups, across every session.
    pub tab_groups: usize,
    /// Sidebar `pin:` keys.
    pub pins: usize,
    /// Sidebar `collapse:` keys.
    pub collapse_keys: usize,
    pub merge_queue: usize,
    pub pr_queue: usize,
    /// Non-terminal dispatches marked `abandoned` (history kept).
    pub dispatches_abandoned: usize,
    /// Non-terminal autopilot runs moved to `stopped` (history kept).
    pub autopilot_stopped: usize,
    pub pool_spares: usize,
    pub pool_targets: usize,
    pub hibernations: usize,
    pub forwards: usize,
    pub shares: usize,
    pub automation_proposals: usize,
}

/// A PR-queue row of the forgotten workspace that a live agent lease holds.
#[derive(Debug)]
pub struct PrQueueLeased {
    pub key: String,
}

impl std::fmt::Display for PrQueueLeased {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "PR-queue entry {} is held by a live agent lease; stop the agent or wait for the lease to expire",
            self.key
        )
    }
}

impl std::error::Error for PrQueueLeased {}

struct PrQueueEntry {
    key: String,
    leased: bool,
}

impl Db {
    /// The slug keying a workspace's tab groups and sidebar state: the
    /// persisted `repo_slugs` row, else the slugified directory name when no
    /// other repo has claimed that slug. `None` means no slug can be named
    /// safely, so slug-keyed cleanup is skipped rather than risk a sibling's
    /// rows.
    pub fn forget_slug(&self, repo_path: &str) -> Result<Option<String>> {
        if let Some(slug) = self.repo_slug_lookup(repo_path)? {
            return Ok(Some(slug));
        }
        let base = Path::new(repo_path)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let base = base
            .strip_suffix(".git")
            .map(str::to_string)
            .unwrap_or(base);
        let slug = crate::util::slugify(&base);
        if slug.is_empty() {
            return Ok(None);
        }
        let claimed: bool = self.conn().query_row(
            "SELECT EXISTS(SELECT 1 FROM repo_slugs WHERE slug=?1)",
            params![slug],
            |row| row.get(0),
        )?;
        Ok((!claimed).then_some(slug))
    }

    /// Every registered worktree path of the workspace, plus its home checkout.
    fn forget_worktree_paths(&self, repo_path: &str) -> Result<Vec<String>> {
        let mut stmt = self
            .conn()
            .prepare("SELECT worktree FROM worktrees WHERE repo_path=?1 ORDER BY worktree")?;
        let rows = stmt.query_map(params![repo_path], |row| row.get::<_, String>(0))?;
        let mut paths = rows.collect::<rusqlite::Result<Vec<String>>>()?;
        if !paths.iter().any(|path| path == repo_path) {
            paths.push(repo_path.to_string());
        }
        Ok(paths)
    }

    fn forget_count(&self, sql: &str, args: impl rusqlite::Params) -> Result<usize> {
        let count: i64 = self.conn().query_row(sql, args, |row| row.get(0))?;
        Ok(usize::try_from(count).unwrap_or(0))
    }

    fn forget_ui_keys(&self, scope: &str, prefix: &str, slug: &str) -> Result<usize> {
        let exact = format!("{prefix}:{slug}");
        let children = format!("{exact}/");
        self.forget_count(
            "SELECT COUNT(*) FROM ui_state WHERE scope=?1
               AND (key=?2 OR substr(key, 1, length(?3)) = ?3)",
            params![scope, exact, children],
        )
    }

    fn forget_pr_entries(&self, repo_path: &str, paths: &[String]) -> Result<Vec<PrQueueEntry>> {
        let now_ms = crate::util::now_ms();
        let mut stmt = self.conn().prepare(
            "SELECT key, repo_root, worktree, agent_claim_owner, agent_claim_lease_until_ms
             FROM pr_queue ORDER BY key",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<i64>>(4)?,
            ))
        })?;
        let mut entries = Vec::new();
        for row in rows {
            let (key, repo_root, worktree, owner, lease_until) = row?;
            let owned = repo_root == repo_path
                || worktree
                    .as_deref()
                    .is_some_and(|worktree| paths.iter().any(|path| path == worktree));
            if owned {
                let leased = owner.is_some() && lease_until.is_some_and(|until| until >= now_ms);
                entries.push(PrQueueEntry { key, leased });
            }
        }
        Ok(entries)
    }

    fn forget_autopilot_open(&self, repo_path: &str) -> Result<Vec<(i64, AutopilotState)>> {
        let mut stmt = self
            .conn()
            .prepare("SELECT id, state FROM autopilot_runs WHERE repo_root=?1 ORDER BY id")?;
        let rows = stmt.query_map(params![repo_path], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut open = Vec::new();
        for row in rows {
            let (id, state) = row?;
            let state = AutopilotState::parse(&state);
            if !state.is_terminal() {
                open.push((id, state));
            }
        }
        Ok(open)
    }

    fn forget_open_dispatches(&self, paths: &[String]) -> Result<Vec<(i64, AgentDispatchStatus)>> {
        Ok(self
            .list_dispatches()?
            .into_iter()
            .filter(|dispatch| {
                !dispatch.status.is_terminal() && paths.contains(&dispatch.worktree_path)
            })
            .map(|dispatch| (dispatch.id, dispatch.status))
            .collect())
    }

    fn forget_pool_spare_names(&self, repo_path: &str) -> Result<Vec<String>> {
        let mut stmt = self.conn().prepare(
            "SELECT sandbox_name FROM pool_spares WHERE repo_path=?1 ORDER BY sandbox_name",
        )?;
        let rows = stmt.query_map(params![repo_path], |row| row.get::<_, String>(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<String>>>()?)
    }

    fn forget_merge_rows(&self, paths: &[String]) -> Result<Vec<String>> {
        let mut queued = Vec::new();
        for path in paths {
            let found: Option<String> = self
                .conn()
                .query_row(
                    "SELECT worktree FROM merge_queue WHERE worktree=?1",
                    params![path],
                    |row| row.get(0),
                )
                .optional()?;
            queued.extend(found);
        }
        Ok(queued)
    }

    /// A live PR-agent lease means an agent is still working one of the
    /// workspace's PR-queue rows: refuse rather than pull the row from under it.
    fn forget_refusal(&self, repo_path: &str, paths: &[String]) -> Result<()> {
        if let Some(entry) = self
            .forget_pr_entries(repo_path, paths)?
            .into_iter()
            .find(|entry| entry.leased)
        {
            return Err(anyhow::Error::new(PrQueueLeased { key: entry.key }));
        }
        for path in paths {
            let claimed = self
                .conn()
                .query_row(
                    "SELECT 1 FROM merge_queue WHERE worktree=?1 AND claim_owner IS NOT NULL",
                    params![path],
                    |_| Ok(()),
                )
                .optional()?;
            if claimed.is_some() {
                return Err(anyhow::Error::new(crate::store::MergeClaimActive));
            }
        }
        Ok(())
    }

    /// Read-only: what [`Db::forget_workspace_offline`] would remove for
    /// `target`. Changes nothing.
    pub fn workspace_forget_counts(
        &self,
        target: &ForgetWorkspace<'_>,
    ) -> Result<WorkspaceForgetCounts> {
        let repo = target.repo_path;
        let paths = self.forget_worktree_paths(repo)?;
        let mut counts = WorkspaceForgetCounts {
            worktrees: self.forget_count(
                "SELECT COUNT(*) FROM worktrees WHERE repo_path=?1",
                params![repo],
            )?,
            merge_queue: self.forget_merge_rows(&paths)?.len(),
            pr_queue: self.forget_pr_entries(repo, &paths)?.len(),
            dispatches_abandoned: self.forget_open_dispatches(&paths)?.len(),
            autopilot_stopped: self.forget_autopilot_open(repo)?.len(),
            pool_spares: self.forget_pool_spare_names(repo)?.len(),
            pool_targets: self.forget_count(
                "SELECT COUNT(*) FROM pool_targets WHERE repo_path=?1",
                params![repo],
            )?,
            hibernations: self
                .hibernations()?
                .iter()
                .filter(|row| row.repo_path == repo || paths.contains(&row.worktree_path))
                .count(),
            forwards: self
                .list_forwards()?
                .iter()
                .filter(|row| paths.contains(&row.worktree))
                .count(),
            shares: self
                .list_shares()?
                .iter()
                .filter(|row| paths.contains(&row.worktree))
                .count(),
            automation_proposals: self.forget_count(
                "SELECT COUNT(*) FROM automation_proposals WHERE repo_root=?1",
                params![repo],
            )?,
            ..WorkspaceForgetCounts::default()
        };
        if let Some(slug) = target.slug {
            let prefix = format!("{slug}/");
            counts.tab_groups = self.forget_count(
                "SELECT COUNT(*) FROM tab_groups WHERE substr(name, 1, length(?1)) = ?1",
                params![prefix],
            )?;
            counts.pins = self.forget_ui_keys(target.sidebar_scope, "pin", slug)?;
            counts.collapse_keys = self.forget_ui_keys(target.sidebar_scope, "collapse", slug)?;
        }
        Ok(counts)
    }

    /// Forget one workspace from this database: the registry half of the UI's
    /// keep-files removal (workspace row, worktree rows and their cache
    /// cascade, repo slug, tombstone, active-workspace pointer, sidebar
    /// pin/collapse keys and tab groups of EVERY persisted session) plus the
    /// rows that drive background work on its worktrees.
    ///
    /// Non-terminal dispatches are abandoned first through the checked status
    /// path (each its own transaction; history survives). Everything else
    /// commits in ONE immediate transaction, so a refusal (a claimed merge-queue
    /// row, a leased PR-queue row) leaves the registry untouched.
    pub fn forget_workspace_offline(
        &self,
        target: &ForgetWorkspace<'_>,
    ) -> Result<WorkspaceForgetCounts> {
        let paths = self.forget_worktree_paths(target.repo_path)?;
        // Refuse before abandoning anything; the transaction re-checks.
        self.forget_refusal(target.repo_path, &paths)?;
        let mut abandoned = 0;
        for (id, status) in self.forget_open_dispatches(&paths)? {
            self.set_dispatch_status_checked(
                id,
                status,
                AgentDispatchStatus::Abandoned,
                Some(FORGET_REASON),
            )
            .with_context(|| format!("abandon dispatch {id}"))?;
            abandoned += 1;
        }
        let mut counts =
            self.transaction_immediate(|db| db.forget_workspace_rows(target, &paths))?;
        counts.dispatches_abandoned = abandoned;
        Ok(counts)
    }

    fn forget_workspace_rows(
        &self,
        target: &ForgetWorkspace<'_>,
        paths: &[String],
    ) -> Result<WorkspaceForgetCounts> {
        let repo = target.repo_path;
        let mut counts = self.workspace_forget_counts(target)?;
        counts.dispatches_abandoned = 0;

        self.forget_refusal(repo, paths)?;
        for entry in &self.forget_pr_entries(repo, paths)? {
            self.conn()
                .execute("DELETE FROM pr_queue WHERE key=?1", params![entry.key])?;
        }

        // The removal path records a `merge_queue_events` row (kept as
        // history) and refuses a row a running land/drain has claimed.
        for worktree in self.forget_merge_rows(paths)? {
            self.remove_merge_entry_as(&worktree, FORGET_ACTOR)
                .with_context(|| format!("remove merge-queue row for {worktree}"))?;
        }

        let now_ms = crate::util::now_ms();
        let mut stopped = 0;
        for (id, state) in self.forget_autopilot_open(repo)? {
            if self.transition_autopilot(
                id,
                state,
                AutopilotState::Stopped,
                Some(FORGET_REASON),
                None,
                now_ms,
            )? {
                stopped += 1;
            }
        }
        counts.autopilot_stopped = stopped;

        for name in self.forget_pool_spare_names(repo)? {
            self.delete_pool_spare(&name)?;
        }
        self.conn()
            .execute("DELETE FROM pool_targets WHERE repo_path=?1", params![repo])?;
        for row in self.hibernations()? {
            if row.repo_path == repo || paths.contains(&row.worktree_path) {
                self.delete_hibernation(&row.worktree_path)?;
            }
        }
        for row in self.list_forwards()? {
            if paths.contains(&row.worktree) {
                self.delete_forward(&row.worktree, row.container_port)?;
            }
        }
        for row in self.list_shares()? {
            if paths.contains(&row.worktree) {
                self.delete_share(&row.worktree, row.local_port)?;
            }
        }
        self.conn().execute(
            "DELETE FROM automation_proposals WHERE repo_root=?1",
            params![repo],
        )?;

        if let Some(slug) = target.slug {
            self.delete_tab_groups_with_prefix(slug)?;
            for prefix in ["collapse", "pin"] {
                self.del_ui_state(target.sidebar_scope, &format!("{prefix}:{slug}"))?;
                self.del_ui_state_prefix(target.sidebar_scope, &format!("{prefix}:{slug}/"))?;
            }
        }

        // The registry half, as the UI's keep-files removal does it.
        self.del_worktrees_for_repo(repo)?;
        self.del_workspace(repo)?;
        self.del_repo_slug(repo)?;
        self.tombstone_workspace(repo)?;
        if self.active_workspace()?.as_deref() == Some(repo) {
            self.del_ui_state("", "active_workspace")?;
        }
        Ok(counts)
    }

    /// `PRAGMA user_version` of this database.
    pub fn user_version(&self) -> Result<i64> {
        Ok(self
            .conn()
            .query_row("PRAGMA user_version", [], |row| row.get(0))?)
    }

    /// Write a consistent copy of this database to `dest` with SQLite's own
    /// `VACUUM INTO` (it reads through this connection, so a WAL database is
    /// copied with its committed WAL content, never as raw files), then
    /// verify the copy opens read-only, passes `quick_check`, and carries the
    /// same `user_version`. `dest` must not exist yet.
    pub fn backup_to(&self, dest: &Path) -> Result<()> {
        if dest.exists() {
            anyhow::bail!("backup destination {} already exists", dest.display());
        }
        if let Some(dir) = dest.parent() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("create backup directory {}", dir.display()))?;
        }
        let dest_text = dest
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("backup path {} is not UTF-8", dest.display()))?;
        self.conn()
            .execute("VACUUM INTO ?1", params![dest_text])
            .with_context(|| format!("write backup {}", dest.display()))?;
        let expected = self.user_version()?;
        let copy =
            rusqlite::Connection::open_with_flags(dest, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                .with_context(|| format!("open backup {}", dest.display()))?;
        let check: String = copy.query_row("PRAGMA quick_check", [], |row| row.get(0))?;
        if check != "ok" {
            anyhow::bail!("backup {} failed quick_check: {check}", dest.display());
        }
        let actual: i64 = copy.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if actual != expected {
            anyhow::bail!(
                "backup {} has user_version {actual}, expected {expected}",
                dest.display()
            );
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "db_workspace_forget_tests.rs"]
mod tests;
