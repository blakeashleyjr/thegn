//! The resident-workspace pool: parked workspaces whose panes stay live.
//!
//! thegn is its own multiplexer, so switching workspaces **parks** the outgoing
//! one — its center pane trees are stashed here and its `PtyPane`s stay live in
//! the global `Panes` table, so switching back reattaches
//! the still-running processes instantly (no DB resurrect, no respawn).
//!
//! Left unbounded that is a slow resource leak: every workspace ever visited
//! keeps one PTY master fd + reader thread + child per pane, forever. Over a
//! long session across many workspaces and terminals the process approaches its
//! open-fd limit, at which point every git read fails at once and the panel
//! header collapses to "—". So the pool is **bounded** by
//! `[session].resident_pool_limit`, evicting the least-recently-used parked
//! workspace once the cap is exceeded — the same bounded-pool shape as
//! [`crate::drawer_state::DrawerPool`].
//!
//! Eviction (and the `limit = 0` reap-on-switch) drops each evicted pane
//! through [`Panes::detach_pane`](crate::panes::Panes::detach_pane), **not** a
//! bare `table.remove`: a daemon-backed pane is marked detach-on-drop so its
//! server-side session keeps running in the pane daemon (the next visit
//! warm-reattaches it) rather than being killed — only in-process PTY panes
//! actually die on eviction. An evicted workspace re-resurrects on the next
//! visit, reattaching live daemon sessions and respawning any in-process panes.

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
    /// panes to detach from the global table when this workspace is evicted.
    fn pane_ids(&self) -> Vec<u32> {
        self.worktrees
            .iter()
            .flat_map(|g| g.tabs.iter())
            .flat_map(|t| t.center.pane_ids())
            .collect()
    }
}

impl WorkspacePool {
    /// Every live pane id across ALL parked workspaces. Their panes stay live in
    /// the global table but are absent from the active `Session`, so the quit
    /// detach/kill sweep must include them or a parked workspace's daemon-backed
    /// sessions are silently killed instead of persisted (or leaked instead of
    /// killed) on quit.
    pub(crate) fn parked_pane_ids(&self) -> Vec<u32> {
        self.parked
            .iter()
            .flat_map(|(_, rw)| rw.pane_ids())
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

    /// Every parked workspace's worktree groups. Their `PtyPane`s stay live in
    /// the global `Panes` table, so the sidebar's dynamic-title collector reads
    /// their OSC window titles the same way it reads the active session's —
    /// keeping a parked (unfocused) workspace's dynamic names fresh, not just
    /// last-known.
    pub(crate) fn resident_groups(&self) -> impl Iterator<Item = &crate::session::WorktreeGroup> {
        self.parked.iter().flat_map(|(_, rw)| rw.worktrees.iter())
    }

    /// Restore a parked workspace, removing it from the pool (it becomes the
    /// active workspace, which is never held here).
    pub(crate) fn take(&mut self, repo: &str) -> Option<ResidentWorkspace> {
        let idx = self.parked.iter().position(|(k, _)| k == repo)?;
        self.parked.remove(idx).map(|(_, rw)| rw)
    }

    /// Remove and return the parked **terminal** group named `name`, with the
    /// key of the workspace that was holding it.
    ///
    /// A terminal's identity is global — the `terminals` table is not scoped to
    /// a repo, so its sidebar row renders in every workspace — but until now its
    /// *residency* was per-workspace: the group lived in whichever session
    /// created it and parked with that session on a switch. Activating the row
    /// from another workspace therefore missed
    /// (`sidebar::build_rows`' `position(|w| w.name == t.name)` only sees the
    /// ACTIVE session) and forked a second group with an empty `pane_sessions`,
    /// which materialized as a brand-new shell while the original stayed alive
    /// and unreachable in the parked trees. Migrating the group instead keeps
    /// one terminal = one shell: its `PtyPane`s are already live in the global
    /// table under these same ids, so the move alone restores it — no
    /// materialize, no respawn, scrollback intact.
    ///
    /// Worktree groups are never migrated: they belong to their repo, and the
    /// `Workspace` row target switches to that workspace instead.
    pub(crate) fn take_terminal_group(
        &mut self,
        name: &str,
    ) -> Option<(String, crate::session::WorktreeGroup)> {
        let (pi, gi) = self.parked.iter().enumerate().find_map(|(pi, (_, rw))| {
            rw.worktrees
                .iter()
                .position(|g| g.name == name && g.kind == crate::session::GroupKind::Terminal)
                .map(|gi| (pi, gi))
        })?;
        let (key, rw) = self.parked.get_mut(pi)?;
        let key = key.clone();
        let group = rw.worktrees.remove(gi);
        // `active` is an INDEX into the vec we just shortened: a group removed
        // at or before it shifts every later group down one, so replaying the
        // old index would land the user on their neighbour on the next visit.
        if gi < rw.active {
            rw.active -= 1;
        }
        rw.active = rw.active.min(rw.worktrees.len().saturating_sub(1));
        // A workspace whose only group was this terminal has nothing left to
        // park; leaving the empty shell behind would make `contains` claim it is
        // resident and a warm switch restore a session with no groups.
        if rw.worktrees.is_empty() {
            self.parked.remove(pi);
        }
        Some((key, group))
    }

    /// Park `rw` under `repo`, enforcing the configured limit. A limit of 0
    /// detaches the workspace's panes immediately (no pooling); an unset limit
    /// (`None`) keeps every entry (unbounded); otherwise the least-recently
    /// parked entries beyond the limit are evicted and their panes dropped from
    /// the table via [`Panes::detach_pane`] (daemon sessions survive; in-process
    /// PTYs die). Re-parking an already-present key replaces it in place (its
    /// live panes are the same ids, so they are not dropped).
    pub(crate) fn stash(&mut self, repo: String, rw: ResidentWorkspace, panes: &mut Panes) {
        if self.limit == Some(0) {
            for id in rw.pane_ids() {
                panes.detach_pane(id);
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
                        panes.detach_pane(id);
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
        remap_group_ids(g, panes);
    }
}

/// Move ONE group's persisted pane ids onto a fresh disjoint range (the
/// per-group body of [`remap_cold_workspace_ids`], shared with the cold
/// terminal restore in `handlers::sidebar_activate`).
///
/// All four id-keyed side maps travel with the tree. That is the whole point:
/// `pane_sessions` is what `materialize_with_specs` reads to warm-reattach a
/// live daemon session instead of forking a fresh shell, and `pane_scrollback`
/// is what repaints the tail when the reattach can't be had — leaving either
/// under the OLD key silently downgrades a restore into a blank new terminal.
pub(crate) fn remap_group_ids(g: &mut crate::session::WorktreeGroup, panes: &mut Panes) {
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
        // Scrollback is keyed by pane id too; without this remap the
        // persisted scrollback stays under the OLD id and is lost when the
        // resurrected pane reads it under its new id (data loss on the
        // cold-workspace id-collision-avoidance path).
        tab.pane_scrollback = std::mem::take(&mut tab.pane_scrollback)
            .into_iter()
            .map(|(id, s)| (map.get(&id).copied().unwrap_or(id), s))
            .collect();
    }
}
