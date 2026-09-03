//! Frozen sort keys: the snapshot that holds worktree order still while the
//! user is navigating the sidebar.
//!
//! Two of the five sort modes are **computed** — `Attention` reads
//! `SidebarStatus::attention_ranks` and `Live` reads
//! `SidebarStatus::activity_recency`, both recomputed off-loop on the hydration
//! thread (~5s). Their whole point is that rows move when the world moves; the
//! problem is that they moved *under the cursor*, mid-keystroke, while the user
//! was walking the list. So the keys — not the rows, not the cursor — are what
//! gets held: while a freeze is armed the sorts read this snapshot instead of
//! the live maps, and the order simply cannot change.
//!
//! Pure data plus a pure lookup. **No clock and no timer live here**: the
//! caller (`SidebarState::rebuild`, and the arm sites in the event loop) decides
//! when a freeze starts and ends, so this module adds no wake source and costs
//! nothing at idle — the 0%-idle contract in CLAUDE.md.
//!
//! The cursor is re-anchored by identity across a resort independently
//! (`SidebarState::rebuild`); that keeps `d`/`Enter` pointed at the right row
//! when the order *does* change. This is the complementary half: while you are
//! navigating, it doesn't change at all.

use std::collections::BTreeMap;

use crate::sidebar::SidebarStatus;

/// How long the order stays held after an `Alt+↑/↓` / `Alt+<digit>` jump made
/// from a **pane** — the one navigation path where the sidebar never takes
/// focus, so the focus-edge freeze cannot cover it.
///
/// 2s deliberately outlives one 500ms hydration tick with room to spare, so the
/// jump and the frame that would have re-ranked underneath it are never in the
/// same window. It is not a "how long until it settles" knob: with the sidebar
/// focused the freeze is held by focus, not by this.
pub(crate) const FREEZE_GRACE: std::time::Duration = std::time::Duration::from_millis(2000);

/// A snapshot of the live sort-key maps as they were when the freeze armed —
/// i.e. the order that is currently **on screen**.
///
/// Captured on the focus-gain edge rather than inside `rebuild` precisely so it
/// is the on-screen order: a hydration landing in the same loop iteration has
/// already swapped `SidebarStatus`, and capturing from that would pin the order
/// one hop *after* the user watched it move.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct SortFreeze {
    /// Snapshot of `SidebarStatus::attention_ranks` (worktree path → rank).
    ranks: BTreeMap<String, u32>,
    /// Snapshot of `SidebarStatus::activity_recency` (worktree path → unix secs).
    recency: BTreeMap<String, f64>,
    /// Snapshot of the per-workspace tier behind `[ui] sidebar_project_sort =
    /// "attention"` (slug → tier). Without this the *project blocks* still
    /// slide under the cursor even though the worktrees inside them are held.
    workspace_tier: BTreeMap<String, u8>,
}

impl SortFreeze {
    /// Snapshot every key the computed sorts read.
    pub(crate) fn capture(status: &SidebarStatus) -> Self {
        Self {
            ranks: status.attention_ranks.clone(),
            recency: status.activity_recency.clone(),
            workspace_tier: status
                .workspace_attention
                .iter()
                .map(|(slug, score)| (slug.clone(), score.tier as u8))
                .collect(),
        }
    }
}

/// The key source [`crate::sidebar::sort_groups`] and its flat counterpart read:
/// the freeze when one is armed, falling back **per key** to the live map on a
/// miss.
///
/// A struct with methods rather than a pair of `&BTreeMap`s so the miss-fallback
/// rule (see [`SortKeys::recency`]) lives in exactly one place and can be tested
/// on its own.
pub(crate) struct SortKeys<'a> {
    frozen: Option<&'a SortFreeze>,
    live_ranks: &'a BTreeMap<String, u32>,
    live_recency: &'a BTreeMap<String, f64>,
    live_workspace: &'a BTreeMap<String, thegn_core::attention::AttentionScore>,
}

impl<'a> SortKeys<'a> {
    /// Build the lookup for one `build_rows` pass. `frozen: None` is exactly the
    /// live maps — the sorts behave as they always did.
    pub(crate) fn new(frozen: Option<&'a SortFreeze>, status: &'a SidebarStatus) -> Self {
        Self {
            frozen,
            live_ranks: &status.attention_ranks,
            live_recency: &status.activity_recency,
            live_workspace: &status.workspace_attention,
        }
    }

    /// Attention rank for a worktree path; `u32::MAX` (last) when nothing knows it.
    pub(crate) fn rank(&self, path: &str) -> u32 {
        self.frozen
            .and_then(|f| f.ranks.get(path))
            .or_else(|| self.live_ranks.get(path))
            .copied()
            .unwrap_or(u32::MAX)
    }

    /// Last-active time for a worktree path; `f64::MIN` (last) when nothing
    /// knows it.
    ///
    /// **A path missing from the freeze falls through to the live map**, and
    /// that is deliberate. The freeze's contract is "a row that is on screen
    /// does not move", not "the list cannot grow": a worktree created during the
    /// freeze was by definition not on screen, so there is no order to protect.
    /// Treating a miss as "never active" would exile a just-created worktree to
    /// the bottom for the whole freeze window — the exact opposite of what
    /// `Live` promises. The freeze itself is never mutated to absorb newly-seen
    /// paths; a time-varying snapshot would defeat the point.
    pub(crate) fn recency(&self, path: &str) -> f64 {
        self.frozen
            .and_then(|f| f.recency.get(path))
            .or_else(|| self.live_recency.get(path))
            .copied()
            .unwrap_or(f64::MIN)
    }

    /// Workspace attention tier for a slug; `u8::MAX` (last) when unknown.
    /// Same fallback rule as [`Self::recency`].
    pub(crate) fn workspace_tier(&self, slug: &str) -> u8 {
        self.frozen
            .and_then(|f| f.workspace_tier.get(slug).copied())
            .or_else(|| self.live_workspace.get(slug).map(|s| s.tier as u8))
            .unwrap_or(u8::MAX)
    }
}

/// Arm the freeze from what is currently on screen.
///
/// No-op when disabled by config, when the sort mode cannot move rows on its own
/// (`SortMode::is_computed`), and — importantly — **when one is already armed**:
/// re-capturing on every keystroke would track the live keys exactly and freeze
/// nothing. Holding the original snapshot is the whole mechanism.
pub(crate) fn arm(sb: &mut crate::handlers::sidebar_persist::SidebarState, status: &SidebarStatus) {
    if !sb.freeze_sort || !sb.view.sort.is_computed() || sb.view.freeze.is_some() {
        return;
    }
    sb.view.freeze = Some(std::sync::Arc::new(SortFreeze::capture(status)));
}

/// Drop the freeze and any pending grace, so the next rebuild re-ranks from the
/// live keys.
pub(crate) fn thaw(sb: &mut crate::handlers::sidebar_persist::SidebarState) {
    sb.view.freeze = None;
    sb.freeze_until = None;
}

/// The order legitimately changed (new sort mode, new filter, a manual
/// reorder): drop the stale snapshot and — if the user is still on the bar —
/// take a fresh one, so the *new* order is what gets held.
///
/// A bare [`thaw`] would be wrong at these sites. They all fire while the
/// sidebar has focus, and `arm` only ever fires on the focus-GAIN edge, so
/// thawing alone would leave the order live-updating until the user left the
/// bar and came back — reintroducing the exact churn the freeze exists to stop.
/// The capture happens before the rebuild, so the fresh snapshot holds the live
/// keys, the rebuild renders the new order from them, and it then stays put.
pub(crate) fn rearm(
    sb: &mut crate::handlers::sidebar_persist::SidebarState,
    status: &SidebarStatus,
) {
    thaw(sb);
    if sb.focused {
        arm(sb, status);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use thegn_core::attention::{AttentionScore, AttentionTier};

    fn status() -> SidebarStatus {
        let mut s = SidebarStatus::default();
        s.attention_ranks.insert("/a".into(), 7);
        s.activity_recency.insert("/a".into(), 100.0);
        s.workspace_attention.insert(
            "repo".into(),
            AttentionScore {
                tier: AttentionTier::Working,
                ..Default::default()
            },
        );
        s
    }

    #[test]
    fn an_unarmed_lookup_is_exactly_the_live_map() {
        let s = status();
        let keys = SortKeys::new(None, &s);
        assert_eq!(keys.rank("/a"), 7);
        assert_eq!(keys.recency("/a"), 100.0);
        assert_eq!(keys.workspace_tier("repo"), AttentionTier::Working as u8);
        // Unknown keys sort last in every axis.
        assert_eq!(keys.rank("/nope"), u32::MAX);
        assert_eq!(keys.recency("/nope"), f64::MIN);
        assert_eq!(keys.workspace_tier("nope"), u8::MAX);
    }

    #[test]
    fn capture_snapshots_all_three_maps() {
        let f = SortFreeze::capture(&status());
        assert_eq!(f.ranks.get("/a"), Some(&7));
        assert_eq!(f.recency.get("/a"), Some(&100.0));
        assert_eq!(
            f.workspace_tier.get("repo"),
            Some(&(AttentionTier::Working as u8))
        );
    }

    #[test]
    fn frozen_keys_win_over_the_live_map() {
        let old = status();
        let f = SortFreeze::capture(&old);
        // The world moves on: new rank, newer activity, more urgent workspace.
        let mut fresh = old.clone();
        fresh.attention_ranks.insert("/a".into(), 0);
        fresh.activity_recency.insert("/a".into(), 999.0);
        fresh.workspace_attention.insert(
            "repo".into(),
            AttentionScore {
                tier: AttentionTier::Blocked,
                ..Default::default()
            },
        );

        let keys = SortKeys::new(Some(&f), &fresh);
        assert_eq!(keys.rank("/a"), 7, "frozen rank must survive a re-rank");
        assert_eq!(keys.recency("/a"), 100.0);
        assert_eq!(keys.workspace_tier("repo"), AttentionTier::Working as u8);
    }

    /// The B6 contract: a worktree the freeze never saw is not on screen, so it
    /// has no order to protect and must use its real key.
    #[test]
    fn a_path_missing_from_the_freeze_falls_back_to_the_live_map() {
        let f = SortFreeze::capture(&status());
        let mut fresh = status();
        fresh.attention_ranks.insert("/new".into(), 0);
        fresh.activity_recency.insert("/new".into(), 999.0);
        fresh.workspace_attention.insert(
            "new-repo".into(),
            AttentionScore {
                tier: AttentionTier::Blocked,
                ..Default::default()
            },
        );

        let keys = SortKeys::new(Some(&f), &fresh);
        assert_eq!(keys.rank("/new"), 0, "a new worktree uses its live rank");
        assert_eq!(
            keys.recency("/new"),
            999.0,
            "a just-created worktree must not be exiled to the bottom"
        );
        assert_eq!(
            keys.workspace_tier("new-repo"),
            AttentionTier::Blocked as u8
        );
        // ...while the rows that WERE on screen stay put.
        assert_eq!(keys.recency("/a"), 100.0);
    }
}
