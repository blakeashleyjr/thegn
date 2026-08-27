//! Re-keying the event loop's `(…, tab index)`-keyed state when a tab is
//! removed from a group.
//!
//! A dozen loop-locals are keyed **positionally** — `(group name, tab index)`
//! for the loading splash / materialize / prewarm bookkeeping, `(group index,
//! tab index)` for the respawn crash counter and the startup-shell watchdog.
//! `Vec::remove` on a group's tab list shifts every tab to the right of the
//! removed one down by one, so after a close those keys silently name a
//! **different tab** — and the closed tab's own key now names its right-hand
//! neighbour.
//!
//! That is not a cosmetic drift. `model.load_steps` is derived every frame from
//! `loading_state[(active group, active tab index)]`, and
//! [`crate::handlers::startup_watchdog::tick`] arms whenever those steps are in
//! the shell-wait shape. Inheriting a closed tab's shell-wait splash therefore
//! points the watchdog at the surviving neighbour's LIVE, long-lived pane —
//! whose `pane_age` is trivially past the deadline — and the watchdog's remedy
//! is to drop that pane and spawn a clean rc-free shell in its place. The user
//! closes a tab and the tab to its right loses its running program and comes
//! back as a bare prompt. (`shell_watchdog_fired` drifts by the same rule, so
//! the fire-at-most-once guard does not protect the neighbour either.)
//!
//! The fix is to rewrite the keys with the same rule `Vec::remove` applies:
//! drop the closed tab's entries, shift everything to its right down one, leave
//! everything to its left (and every other group) alone. Pure map surgery — no
//! I/O, no session access — so it is unit-tested directly.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

/// Where a `(group, tab index)` key lands after tab `closed` is removed from
/// `group`: unchanged for another group or a tab to the LEFT, `None` (dropped)
/// for the closed tab itself, one lower for a tab to its RIGHT.
///
/// The `Greater` arm's `ti - 1` cannot underflow — `ti > closed >= 0` — but it
/// is written as an explicit `cmp` rather than a subtraction on a predicate so
/// the three cases are exhaustive by construction.
pub(crate) fn shifted_index(g: &str, ti: usize, group: &str, closed: usize) -> Option<usize> {
    if g != group {
        return Some(ti);
    }
    match ti.cmp(&closed) {
        Ordering::Less => Some(ti),
        Ordering::Equal => None,
        Ordering::Greater => Some(ti - 1),
    }
}

/// [`shifted_index`] for the `(group index, tab index)`-keyed state.
fn shifted_index_gi(g: usize, ti: usize, gi: usize, closed: usize) -> Option<usize> {
    if g != gi {
        return Some(ti);
    }
    match ti.cmp(&closed) {
        Ordering::Less => Some(ti),
        Ordering::Equal => None,
        Ordering::Greater => Some(ti - 1),
    }
}

/// Re-key a `(group name, tab index)` set after tab `closed` left `group`.
pub(crate) fn shift_named_set(set: &mut HashSet<(String, usize)>, group: &str, closed: usize) {
    let taken = std::mem::take(set);
    *set = taken
        .into_iter()
        .filter_map(|(g, ti)| shifted_index(&g, ti, group, closed).map(|ti| (g, ti)))
        .collect();
}

/// Re-key a `(group name, tab index)` map after tab `closed` left `group`.
pub(crate) fn shift_named_map<V>(
    map: &mut HashMap<(String, usize), V>,
    group: &str,
    closed: usize,
) {
    let taken = std::mem::take(map);
    *map = taken
        .into_iter()
        .filter_map(|((g, ti), v)| shifted_index(&g, ti, group, closed).map(|ti| ((g, ti), v)))
        .collect();
}

/// Re-key a `(group index, tab index)` set after tab `closed` left group `gi`.
pub(crate) fn shift_indexed_set(set: &mut HashSet<(usize, usize)>, gi: usize, closed: usize) {
    let taken = std::mem::take(set);
    *set = taken
        .into_iter()
        .filter_map(|(g, ti)| shifted_index_gi(g, ti, gi, closed).map(|ti| (g, ti)))
        .collect();
}

/// Re-key a `(group index, tab index)` map after tab `closed` left group `gi`.
pub(crate) fn shift_indexed_map<V>(map: &mut HashMap<(usize, usize), V>, gi: usize, closed: usize) {
    let taken = std::mem::take(map);
    *map = taken
        .into_iter()
        .filter_map(|((g, ti), v)| shifted_index_gi(g, ti, gi, closed).map(|ti| ((g, ti), v)))
        .collect();
}

/// Every tab-index-keyed loop-local the close paths must re-key, borrowed for
/// one call. Kept as one struct so a close site cannot re-key half of them —
/// the half-updated state is what produced the watchdog crossfire above.
pub(crate) struct TabScopedState<'a> {
    pub loading_state: &'a mut crate::loading::track::LoadingTracker,
    pub loading_remote: &'a mut HashMap<(String, usize), bool>,
    pub loading_retired: &'a mut HashSet<(String, usize)>,
    pub materialize_inflight: &'a mut HashSet<(String, usize)>,
    pub materialize_failed: &'a mut HashSet<(String, usize)>,
    pub prewarm_inflight: &'a mut HashSet<(String, usize)>,
    pub prewarm_failed: &'a mut HashSet<(String, usize)>,
    pub halt_dismissed: &'a mut HashSet<(String, usize)>,
    pub creating_tabs: &'a mut HashSet<(String, usize)>,
    pub respawn_crash_count: &'a mut HashMap<(usize, usize), u32>,
    pub shell_watchdog_fired: &'a mut HashSet<(usize, usize)>,
    pub shell_watchdog_extended: &'a mut HashSet<(usize, usize)>,
}

impl TabScopedState<'_> {
    /// Tab `closed` was removed from `group` (at session index `gi`): drop its
    /// entries and shift the tabs to its right down one, so no surviving tab
    /// inherits state that was addressed to a different tab.
    pub(crate) fn on_tab_closed(&mut self, group: &str, gi: usize, closed: usize) {
        self.loading_state.on_tab_closed(group, closed);
        shift_named_map(self.loading_remote, group, closed);
        shift_named_set(self.loading_retired, group, closed);
        shift_named_set(self.materialize_inflight, group, closed);
        shift_named_set(self.materialize_failed, group, closed);
        shift_named_set(self.prewarm_inflight, group, closed);
        shift_named_set(self.prewarm_failed, group, closed);
        shift_named_set(self.halt_dismissed, group, closed);
        shift_named_set(self.creating_tabs, group, closed);
        shift_indexed_map(self.respawn_crash_count, gi, closed);
        shift_indexed_set(self.shell_watchdog_fired, gi, closed);
        shift_indexed_set(self.shell_watchdog_extended, gi, closed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(g: &str, ti: usize) -> (String, usize) {
        (g.to_string(), ti)
    }

    #[test]
    fn shifted_index_drops_the_closed_tab_and_pulls_the_right_side_down() {
        assert_eq!(shifted_index("a", 0, "a", 1), Some(0), "left of closed");
        assert_eq!(shifted_index("a", 1, "a", 1), None, "the closed tab");
        assert_eq!(shifted_index("a", 2, "a", 1), Some(1), "right of closed");
        assert_eq!(shifted_index("b", 2, "a", 1), Some(2), "another group");
        // Closing the leftmost tab must not underflow the tab-0 key: it is the
        // closed one (dropped), and everything else moves down.
        assert_eq!(shifted_index("a", 0, "a", 0), None);
        assert_eq!(shifted_index("a", 1, "a", 0), Some(0));
    }

    /// The regression: a closed tab's loading/watchdog state must NOT be
    /// inherited by the tab that slides into its slot. Before the fix, closing
    /// tab 1 left `("a", 1)` in place — now naming the former tab 2 — so the
    /// surviving tab picked up the closed tab's shell-wait splash and the
    /// startup watchdog replaced its live pane with a clean shell.
    #[test]
    fn closing_a_tab_never_hands_its_state_to_the_tab_on_its_right() {
        let mut set: HashSet<(String, usize)> = [k("a", 0), k("a", 1), k("a", 2), k("b", 1)]
            .into_iter()
            .collect();
        shift_named_set(&mut set, "a", 1);
        assert!(set.contains(&k("a", 0)), "left of the close is untouched");
        assert!(
            !set.contains(&k("a", 2)),
            "the old right-hand key is rewritten, not left dangling"
        );
        assert!(set.contains(&k("a", 1)), "former tab 2 is now tab 1");
        assert_eq!(set.len(), 3, "the closed tab's own entry is dropped");
        assert!(set.contains(&k("b", 1)), "another group is untouched");
    }

    #[test]
    fn named_map_values_ride_along_with_their_shifted_key() {
        let mut m: HashMap<(String, usize), bool> = [(k("a", 1), true), (k("a", 2), false)]
            .into_iter()
            .collect();
        shift_named_map(&mut m, "a", 1);
        assert_eq!(m.get(&k("a", 1)), Some(&false), "tab 2's value moved down");
        assert_eq!(m.len(), 1);
    }

    #[test]
    fn indexed_keys_shift_only_within_the_closing_group() {
        let mut set: HashSet<(usize, usize)> =
            [(0, 0), (0, 1), (0, 2), (1, 2)].into_iter().collect();
        shift_indexed_set(&mut set, 0, 1);
        assert!(set.contains(&(0, 0)));
        assert!(set.contains(&(0, 1)), "former (0,2) is now (0,1)");
        assert!(!set.contains(&(0, 2)));
        assert!(set.contains(&(1, 2)), "a different group is untouched");

        let mut m: HashMap<(usize, usize), u32> = [((0, 2), 3), ((1, 0), 9)].into_iter().collect();
        shift_indexed_map(&mut m, 0, 1);
        assert_eq!(m.get(&(0, 1)), Some(&3));
        assert_eq!(m.get(&(1, 0)), Some(&9));
    }

    // --- The close-targeting regression, at the session + loop-state level. ---

    use crate::center::CenterTree;
    use crate::chrome::{LoadStep, StepKind};
    use crate::session::{CloseResult, GroupKind, Session, WorktreeGroup};

    fn shell_wait() -> Vec<LoadStep> {
        vec![
            LoadStep::done("sandbox").with_kind(StepKind::Resolve),
            LoadStep::active("shell").with_kind(StepKind::Shell),
        ]
    }

    /// A 3-tab group whose tabs own panes 1, 2, 3 (ids are stable and unique —
    /// that is the identity a close must target by).
    fn three_tab_session(active_tab: usize) -> (Session, crate::panes::Panes) {
        let mut session = Session {
            id: "s".into(),
            worktrees: vec![WorktreeGroup::new("app/home", GroupKind::Home, "/tmp/app")],
            active: 0,
        };
        let g = &mut session.worktrees[0];
        g.tabs[0].center = CenterTree::Leaf(1);
        g.tabs[0].focused_pane = 1;
        g.add_tab();
        g.tabs[1].center = CenterTree::Leaf(2);
        g.tabs[1].focused_pane = 2;
        g.add_tab();
        g.tabs[2].center = CenterTree::Leaf(3);
        g.tabs[2].focused_pane = 3;
        g.active_tab = active_tab;

        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        let mut panes = crate::panes::Panes::new(tx);
        for id in [1u32, 2, 3] {
            panes.insert_test_pane(id);
        }
        (session, panes)
    }

    /// Everything a close site must re-key, plus the assertions helper.
    #[derive(Default)]
    struct Loop {
        loading_state: crate::loading::track::LoadingTracker,
        loading_remote: HashMap<(String, usize), bool>,
        loading_retired: HashSet<(String, usize)>,
        materialize_inflight: HashSet<(String, usize)>,
        materialize_failed: HashSet<(String, usize)>,
        prewarm_inflight: HashSet<(String, usize)>,
        prewarm_failed: HashSet<(String, usize)>,
        halt_dismissed: HashSet<(String, usize)>,
        creating_tabs: HashSet<(String, usize)>,
        respawn_crash_count: HashMap<(usize, usize), u32>,
        shell_watchdog_fired: HashSet<(usize, usize)>,
        shell_watchdog_extended: HashSet<(usize, usize)>,
    }

    impl Loop {
        fn on_tab_closed(&mut self, group: &str, gi: usize, closed: usize) {
            TabScopedState {
                loading_state: &mut self.loading_state,
                loading_remote: &mut self.loading_remote,
                loading_retired: &mut self.loading_retired,
                materialize_inflight: &mut self.materialize_inflight,
                materialize_failed: &mut self.materialize_failed,
                prewarm_inflight: &mut self.prewarm_inflight,
                prewarm_failed: &mut self.prewarm_failed,
                halt_dismissed: &mut self.halt_dismissed,
                creating_tabs: &mut self.creating_tabs,
                respawn_crash_count: &mut self.respawn_crash_count,
                shell_watchdog_fired: &mut self.shell_watchdog_fired,
                shell_watchdog_extended: &mut self.shell_watchdog_extended,
            }
            .on_tab_closed(group, gi, closed);
        }
    }

    /// Replay exactly what `handlers::close::close_tab` does, minus the
    /// DB/model tail: capture the target, remove the active tab, drop only that
    /// tab's panes, re-key the loop state.
    fn close_active(session: &mut Session, panes: &mut crate::panes::Panes, lp: &mut Loop) {
        let gi = session
            .active
            .min(session.worktrees.len().saturating_sub(1));
        let target = session
            .worktrees
            .get(gi)
            .map(|g| (g.name.clone(), gi, g.active_tab));
        if let CloseResult::Tab(tab) = session.close_active_tab() {
            for id in tab.center.pane_ids() {
                panes.table.remove(&id);
            }
            if let Some((name, gi, ti)) = target {
                lp.on_tab_closed(&name, gi, ti);
            }
        }
    }

    /// THE REGRESSION. Closing tab 0 must not hand its shell-wait splash to the
    /// tab that slides into slot 0 — `model.load_steps` is derived from
    /// `loading_state[(group, active tab index)]`, and a live tab wearing a
    /// shell-wait splash is what arms `startup_watchdog::tick` against its
    /// healthy pane (which the watchdog then replaces with a clean rc-free
    /// shell: the reported "tab to the right drops to a bare bash prompt").
    #[test]
    fn closing_a_tab_does_not_hand_its_splash_to_the_tab_on_its_right() {
        let (mut session, mut panes) = three_tab_session(0);
        let mut lp = Loop::default();
        // Tab 0 is bringing up (splash in the shell-wait shape); tabs 1 and 2
        // are long-since live and have no splash at all.
        lp.loading_state.set(k("app/home", 0), shell_wait());
        lp.shell_watchdog_fired.insert((0, 0));

        close_active(&mut session, &mut panes, &mut lp);

        let g = &session.worktrees[0];
        assert_eq!(g.tabs.len(), 2);
        assert_eq!(g.active_tab, 0, "focus lands on the former tab 1");
        assert_eq!(
            g.tabs[0].center,
            CenterTree::Leaf(2),
            "the surviving neighbour keeps its own pane id"
        );
        assert!(
            lp.loading_state.get(&k("app/home", 0)).is_none(),
            "the closed tab's shell-wait splash must not be inherited by its neighbour"
        );
        assert!(
            !lp.shell_watchdog_fired.contains(&(0, 0)),
            "the closed tab's watchdog mark must not follow its neighbour either"
        );
    }

    /// The pane side of the same targeting rule: only the closed tab's panes
    /// leave the table; the neighbour's live pane (and its session binding)
    /// is untouched.
    #[test]
    fn closing_a_tab_reaps_only_its_own_panes() {
        for closed in [0usize, 1, 2] {
            let (mut session, mut panes) = three_tab_session(closed);
            let mut lp = Loop::default();
            let doomed = session.worktrees[0].tabs[closed].center.pane_ids();

            close_active(&mut session, &mut panes, &mut lp);

            for id in [1u32, 2, 3] {
                assert_eq!(
                    panes.table.contains_key(&id),
                    !doomed.contains(&id),
                    "closing tab {closed}: pane {id} liveness"
                );
            }
            let survivors: Vec<u32> = session.worktrees[0]
                .tabs
                .iter()
                .flat_map(|t| t.center.pane_ids())
                .collect();
            for id in &survivors {
                assert!(
                    panes.table.contains_key(id),
                    "closing tab {closed}: surviving tab's pane {id} must stay live"
                );
            }
        }
    }

    /// A surviving tab keeps its OWN state across the shift — the entry moves
    /// down with the tab rather than being dropped or left behind.
    #[test]
    fn a_surviving_tabs_own_state_moves_down_with_it() {
        let (mut session, mut panes) = three_tab_session(0);
        let mut lp = Loop::default();
        lp.loading_state
            .set(k("app/home", 2), vec![LoadStep::active("clone")]);
        lp.materialize_inflight.insert(k("app/home", 2));
        lp.respawn_crash_count.insert((0, 2), 2);

        close_active(&mut session, &mut panes, &mut lp);

        assert_eq!(
            lp.loading_state
                .get(&k("app/home", 1))
                .map(|s| s[0].label.clone()),
            Some("clone".to_string()),
            "former tab 2's splash is now tab 1's"
        );
        assert!(lp.materialize_inflight.contains(&k("app/home", 1)));
        assert!(!lp.materialize_inflight.contains(&k("app/home", 2)));
        assert_eq!(lp.respawn_crash_count.get(&(0, 1)), Some(&2));
    }

    /// Closing the rightmost tab clamps focus to its left neighbour, and the
    /// LAST tab is never removed (the group is the durable surface — closing a
    /// worktree is the separate explicit action).
    #[test]
    fn close_rightmost_clamps_focus_and_the_last_tab_is_refused() {
        let (mut session, mut panes) = three_tab_session(2);
        let mut lp = Loop::default();

        close_active(&mut session, &mut panes, &mut lp);
        assert_eq!(session.worktrees[0].tabs.len(), 2);
        assert_eq!(
            session.worktrees[0].active_tab, 1,
            "clamped, not out of range"
        );

        close_active(&mut session, &mut panes, &mut lp);
        assert_eq!(session.worktrees[0].tabs.len(), 1);
        assert_eq!(session.worktrees[0].active_tab, 0);

        // Down to one tab: refused, and nothing is reaped.
        assert_eq!(session.close_active_tab(), CloseResult::Nothing);
        assert_eq!(session.worktrees[0].tabs.len(), 1);
        assert!(
            panes
                .table
                .contains_key(&session.worktrees[0].tabs[0].focused_pane),
            "the last tab's pane survives a refused close"
        );
    }

    /// Closing the RIGHTMOST tab leaves every surviving key alone (nothing is
    /// to its right) and closing the only remaining index is a plain drop.
    #[test]
    fn closing_the_rightmost_tab_touches_nothing_else() {
        let mut set: HashSet<(String, usize)> = [k("a", 0), k("a", 1)].into_iter().collect();
        shift_named_set(&mut set, "a", 1);
        assert_eq!(set.len(), 1);
        assert!(set.contains(&k("a", 0)));

        let mut only: HashSet<(String, usize)> = [k("a", 0)].into_iter().collect();
        shift_named_set(&mut only, "a", 0);
        assert!(only.is_empty());
    }
}
