//! Priority shedding for the statusbar's right cluster.
//!
//! The masthead has `fit_stats_cluster`; the statusbar had nothing, so when the
//! right cluster overflowed the bar `seg::cut` kept the *head* of the run and
//! dropped the tail — which, given the push order (config widgets first,
//! daemon chip last), meant a long transient status message or a narrow
//! terminal silently removed the daemon chip, `LOCKED`, the CI failure, the
//! MQ flag and the `✋`/`⚑` alarms while `12.3k LOC` kept its cells. The hit
//! table was also built from the unfitted list, so clicks landed on the wrong
//! item.
//!
//! [`fit`] is the single pass every consumer runs (painter, hit-test, ←/→
//! navigation, Enter): it first clips the free-text `status` widget to a
//! budget, then sheds whole items lowest-priority-first until the cluster fits,
//! never touching the items marked [`KEEP`].

use crate::chrome::{BarBadge, BarItemId};
use crate::seg::{Seg, seg_width};

/// Cells the `status` widget may keep when the cluster overflows (it is the
/// one free-text item; everything else is a fixed-width chip).
const STATUS_MIN_COLS: usize = 16;

/// Shed order — lower sheds first. Badges the user can least afford to miss
/// are last; [`KEEP`] never sheds.
const KEEP: u8 = u8::MAX;

fn priority(id: &BarItemId) -> u8 {
    match id {
        BarItemId::Widget(w) => match w.as_str() {
            "loc" => 0,
            "tests" => 1,
            "pr" => 2,
            "disk" => 3,
            "status" => 4,
            _ => 5,
        },
        BarItemId::Badge(b) => match b {
            BarBadge::Media => 10,
            BarBadge::Ingress => 11,
            BarBadge::DiskWarn => 12,
            BarBadge::Network => 13,
            BarBadge::Notifications => 20,
            BarBadge::PrQueue => 21,
            BarBadge::MergeQueue => 22,
            BarBadge::Ci => 23,
            BarBadge::Zoom => 30,
            BarBadge::Maximized => 31,
            BarBadge::Sync => 32,
            BarBadge::Attention => 40,
            // Key-lock: while it is on every chord goes to the pane, and this
            // badge is the only thing that says so.
            BarBadge::Lock => KEEP,
            // The daemon chip's contract is "never silent" (statusbar_badges).
            BarBadge::Persist => KEEP,
        },
        BarItemId::Help => KEEP,
    }
}

/// Width of the cluster exactly as `chrome::statusbar_right_layout` lays it
/// out: ` │ ` between two adjacent widgets, one space before each badge, one
/// trailing space.
fn cluster_width(items: &[(BarItemId, Vec<Seg>)]) -> usize {
    let mut w = 1; // trailing space
    let mut prev_widget = false;
    for (id, segs) in items {
        let is_widget = matches!(id, BarItemId::Widget(_));
        w += match (is_widget, prev_widget) {
            (true, true) => 3,
            (true, false) => 0,
            (false, _) => 1,
        };
        w += seg_width(segs);
        prev_widget = is_widget;
    }
    w
}

/// Fit `items` into `avail` cells. Returns the surviving items in their
/// original order — the list the painter, the hit-tester and the keyboard
/// cursor all share.
pub fn fit(mut items: Vec<(BarItemId, Vec<Seg>)>, avail: usize) -> Vec<(BarItemId, Vec<Seg>)> {
    if cluster_width(&items) <= avail {
        return items;
    }
    // 1. Clip the free-text status widget.
    let excess = cluster_width(&items).saturating_sub(avail);
    if excess > 0
        && let Some(pos) = items
            .iter()
            .position(|(id, _)| matches!(id, BarItemId::Widget(w) if w == "status"))
    {
        let cur = seg_width(&items[pos].1);
        let target = cur.saturating_sub(excess).max(STATUS_MIN_COLS.min(cur));
        if target < cur {
            items[pos].1 = crate::seg::cut(&items[pos].1, target);
        }
    }
    // 2. Shed whole items, lowest priority first, until the cluster fits.
    while cluster_width(&items) > avail {
        let Some((pos, _)) = items
            .iter()
            .enumerate()
            .filter(|(_, (id, _))| priority(id) != KEEP)
            .min_by_key(|(_, (id, _))| priority(id))
        else {
            break;
        };
        items.remove(pos);
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::seg::{Tok, seg};

    fn widget(id: &str, text: &str) -> (BarItemId, Vec<Seg>) {
        (
            BarItemId::Widget(id.into()),
            vec![seg(Tok::Slot(crate::chrome::S::Dim), text)],
        )
    }
    fn badge(b: BarBadge, text: &str) -> (BarItemId, Vec<Seg>) {
        (
            BarItemId::Badge(b),
            vec![Seg::chip(Tok::Slot(crate::chrome::S::Dim), text)],
        )
    }
    fn ids(v: &[(BarItemId, Vec<Seg>)]) -> Vec<BarItemId> {
        v.iter().map(|(id, _)| id.clone()).collect()
    }

    #[test]
    fn nothing_changes_when_it_fits() {
        let items = vec![widget("loc", "12.3k LOC"), badge(BarBadge::Persist, " ◆ ")];
        assert_eq!(ids(&fit(items.clone(), 200)), ids(&items));
    }

    /// The alarms survive; the LOC count and the long status go first.
    #[test]
    fn sheds_widgets_before_badges_and_never_the_keepers() {
        let items = vec![
            widget("loc", "12.3k LOC"),
            widget("disk", "1.2 GB"),
            widget(
                "status",
                "Persistent session expired; press Enter to relaunch (Esc for a shell)",
            ),
            badge(BarBadge::Attention, " ✋ 2 "),
            badge(BarBadge::Ci, " ✗ CI "),
            badge(BarBadge::MergeQueue, " ⚑ 1 MQ "),
            badge(BarBadge::Lock, " ⌁ LOCKED "),
            badge(BarBadge::Persist, " ◆ "),
        ];
        let out = ids(&fit(items, 40));
        assert!(
            out.contains(&BarItemId::Badge(BarBadge::Persist)),
            "{out:?}"
        );
        assert!(out.contains(&BarItemId::Badge(BarBadge::Lock)), "{out:?}");
        assert!(
            out.contains(&BarItemId::Badge(BarBadge::Attention)),
            "{out:?}"
        );
        assert!(!out.contains(&BarItemId::Widget("loc".into())), "{out:?}");
        // Order is preserved for what survives.
        let pos = |b| out.iter().position(|i| *i == BarItemId::Badge(b)).unwrap();
        assert!(pos(BarBadge::Attention) < pos(BarBadge::Lock));
        assert!(pos(BarBadge::Lock) < pos(BarBadge::Persist));
    }

    /// A long status message is clipped (with the ellipsis) before anything
    /// else is shed, and survives alongside the badges when that is enough.
    #[test]
    fn long_status_is_clipped_first() {
        let items = vec![
            widget(
                "status",
                "workspace create failed: some very long explanation of what went wrong",
            ),
            badge(BarBadge::Ci, " ✗ CI "),
            badge(BarBadge::Persist, " ◆ "),
        ];
        let out = fit(items, 40);
        let status = out
            .iter()
            .find(|(id, _)| *id == BarItemId::Widget("status".into()))
            .expect("status survives as a clipped item");
        assert!(seg_width(&status.1) < 40);
        assert!(
            status.1.last().unwrap().text.ends_with('…') || seg_width(&status.1) >= STATUS_MIN_COLS
        );
        assert!(
            out.iter()
                .any(|(id, _)| *id == BarItemId::Badge(BarBadge::Ci))
        );
        assert!(cluster_width(&out) <= 40);
    }

    #[test]
    fn keepers_alone_may_still_overflow_but_are_never_dropped() {
        let items = vec![
            badge(BarBadge::Lock, " ⌁ LOCKED "),
            badge(BarBadge::Persist, " ◆ "),
        ];
        let out = fit(items.clone(), 3);
        assert_eq!(ids(&out), ids(&items));
    }
}
