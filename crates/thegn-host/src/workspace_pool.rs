//! The resident-workspace pool: parked workspaces whose panes stay live.
//!
//! thegn is its own multiplexer, so switching workspaces **parks** the outgoing
//! one — its center pane trees are stashed here and its `PtyPane`s stay live in
//! the global [`Panes`](crate::panes::Panes) table, so switching back reattaches
//! the still-running processes instantly (no DB resurrect, no respawn).
//!
//! Left unbounded that is a slow resource leak: every workspace ever visited
//! keeps one PTY master fd + reader thread + child per pane, forever. Over a
//! long session across many workspaces and terminals the process approaches its
//! open-fd limit, at which point every git read fails at once and the panel
//! header collapses to "—". So the pool is **bounded** by
//! `[session].resident_pool_limit`, evicting the least-recently-used parked
//! workspace (and reaping its panes) once the cap is exceeded — the same
//! bounded-pool shape as [`crate::drawer_state::DrawerPool`]. An evicted
//! workspace re-resurrects (respawning its processes) on the next visit.

use std::collections::VecDeque;

use crate::panes::Panes;

/// A workspace parked in the [`WorkspacePool`]: just the center pane trees and
/// the active group index. Its `PtyPane`s stay live in `Panes` (we never reap on
/// a switch), so restoring it reattaches the still-running processes by id. The
/// drawer rides the shared (dir-keyed) `DrawerPool`, so it isn't parked here.
pub(crate) struct ResidentWorkspace {
    pub(crate) worktrees: Vec<crate::session::WorktreeGroup>,
    pub(crate) active: usize,
}

impl ResidentWorkspace {
    /// Every live pane id this workspace owns, across all its groups' tabs — the
    /// panes to reap from the global table when this workspace is evicted.
    fn pane_ids(&self) -> Vec<u32> {
        self.worktrees
            .iter()
            .flat_map(|g| g.tabs.iter())
            .flat_map(|t| t.center.pane_ids())
            .collect()
    }
}

/// Keeps recently-visited workspaces' panes alive in memory, keyed by
/// `repo_path` (`Session::id`). Switching parks the outgoing workspace and
/// restores the target's live panes instead of killing and respawning them.
///
/// Bounded by `[session].resident_pool_limit`: entries are held in
/// recency order (front = least-recently parked, next to evict) and the oldest
/// is reaped once the limit is exceeded, so resident panes cannot accumulate
/// without limit. A limit of `0` disables pooling (a switch reaps immediately).
#[derive(Default)]
pub(crate) struct WorkspacePool {
    /// `(repo-key, parked workspace)` in recency order; front is the oldest.
    parked: VecDeque<(String, ResidentWorkspace)>,
    /// Cap on parked entries. `None` (the `Default`) = unbounded — the safe
    /// pre-feature behavior, so an unconfigured pool never reaps unexpectedly;
    /// the loop calls [`set_limit`](Self::set_limit) from config at startup and
    /// on live reload.
    limit: Option<usize>,
}

impl WorkspacePool {
    /// Set the cap on parked (resident) workspaces from `[session]
    /// resident_pool_limit`. Applied on the next `stash`; lowering it doesn't
    /// retroactively reap (the next park trims down to the new cap).
    pub(crate) fn set_limit(&mut self, limit: usize) {
        self.limit = Some(limit);
    }

    pub(crate) fn contains(&self, repo: &str) -> bool {
        self.parked.iter().any(|(k, _)| k == repo)
    }

    /// Restore a parked workspace, removing it from the pool (it becomes the
    /// active workspace, which is never held here).
    pub(crate) fn take(&mut self, repo: &str) -> Option<ResidentWorkspace> {
        let idx = self.parked.iter().position(|(k, _)| k == repo)?;
        self.parked.remove(idx).map(|(_, rw)| rw)
    }

    /// Park `rw` under `repo`, enforcing the configured limit. A limit of 0
    /// reaps the workspace's panes immediately (no pooling); an unset limit
    /// (`None`) keeps every entry (unbounded); otherwise the least-recently
    /// parked entries beyond the limit are evicted and their panes dropped from
    /// the table. Re-parking an already-present key replaces it in place (its
    /// live panes are the same ids, so they are not reaped).
    pub(crate) fn stash(&mut self, repo: String, rw: ResidentWorkspace, panes: &mut Panes) {
        if self.limit == Some(0) {
            for id in rw.pane_ids() {
                panes.table.remove(&id);
            }
            return;
        }
        // Drop any stale entry for this key without reaping — the new snapshot
        // supersedes it and owns the same live panes.
        if let Some(idx) = self.parked.iter().position(|(k, _)| k == &repo) {
            self.parked.remove(idx);
        }
        self.parked.push_back((repo, rw));
        if let Some(limit) = self.limit {
            while self.parked.len() > limit {
                if let Some((_, evicted)) = self.parked.pop_front() {
                    for id in evicted.pane_ids() {
                        panes.table.remove(&id);
                    }
                }
            }
        }
    }
}

/// Move a freshly cold-resurrected workspace's pane ids onto a disjoint range
/// reserved past every live pane, so its persisted tree can't alias a live pane
/// of another resident workspace (the bleed the old reap-on-switch prevented).
/// `materialize_with_specs` then spawns real panes over these placeholders.
pub(crate) fn remap_cold_workspace_ids(session: &mut crate::session::Session, panes: &mut Panes) {
    for g in &mut session.worktrees {
        for tab in &mut g.tabs {
            let mut uniq = tab.center.pane_ids();
            uniq.sort_unstable();
            uniq.dedup();
            if uniq.is_empty() {
                continue;
            }
            let base = panes.reserve_ids(uniq.len() as u32);
            let map: std::collections::HashMap<u32, u32> = uniq
                .iter()
                .enumerate()
                .map(|(i, &old)| (old, base + i as u32))
                .collect();

            tab.center
                .remap(&mut |id| map.get(&id).copied().unwrap_or(id));
            tab.focused_pane = map
                .get(&tab.focused_pane)
                .copied()
                .unwrap_or(tab.focused_pane);
            tab.pane_cwds = std::mem::take(&mut tab.pane_cwds)
                .into_iter()
                .map(|(id, cwd)| (map.get(&id).copied().unwrap_or(id), cwd))
                .collect();
            tab.pane_cmds = std::mem::take(&mut tab.pane_cmds)
                .into_iter()
                .map(|(id, cmd)| (map.get(&id).copied().unwrap_or(id), cmd))
                .collect();
            tab.pane_sessions = std::mem::take(&mut tab.pane_sessions)
                .into_iter()
                .map(|(id, s)| (map.get(&id).copied().unwrap_or(id), s))
                .collect();
        }
    }
}
