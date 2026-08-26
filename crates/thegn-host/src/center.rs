//! The center pane tree — what replaces zellij's swap layouts. Each tab owns a
//! `CenterTree`: tiled splits (row/column, weighted) and stacks (tabbed, one
//! visible). It serializes to JSON for `tab_layout.pane_tree` (resurrect) and
//! lays out to pane rects deterministically — no flexbox engine needed for the
//! tiling itself.
//!
// The split tree is foundation for the multi-pane center; it's exercised by
// tests and wired into the live render path as Phase 2 grows past one pane.
#![allow(dead_code)]

use serde::{Deserialize, Serialize};

use crate::compositor::Rect;

/// Stable per-pane identifier within a tab.
pub type PaneId = u32;

/// Split axis: `Row` lays children left-to-right (divides columns); `Col` lays
/// them top-to-bottom (divides rows).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Dir {
    Row,
    Col,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum CenterTree {
    /// A single terminal pane filling its rect.
    Leaf(PaneId),
    /// Tiled children with per-child weights (the vertical/horizontal arrangements).
    Split { dir: Dir, children: Vec<Branch> },
    /// Tabbed panes; only `active` is visible and fills the rect (the stacked
    /// arrangement).
    Stack { panes: Vec<PaneId>, active: usize },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Branch {
    pub weight: f32,
    pub child: CenterTree,
}

impl CenterTree {
    /// A fresh single-pane tree.
    pub fn single(pane: PaneId) -> Self {
        CenterTree::Leaf(pane)
    }

    /// Lay the tree out within `rect`, yielding `(pane, rect)` for every visible
    /// pane (stack members other than `active` are omitted — they're suspended).
    pub fn layout(&self, rect: Rect) -> Vec<(PaneId, Rect)> {
        let mut out = Vec::new();
        self.layout_into(rect, &mut out);
        out
    }

    fn layout_into(&self, rect: Rect, out: &mut Vec<(PaneId, Rect)>) {
        match self {
            CenterTree::Leaf(p) => out.push((*p, rect)),
            CenterTree::Stack { panes, active } => {
                if let Some(p) = panes.get(*active).or_else(|| panes.first()) {
                    out.push((*p, rect));
                }
            }
            CenterTree::Split { dir, children } => {
                if children.is_empty() {
                    return;
                }
                let total: f32 = children.iter().map(|b| b.weight.max(0.0)).sum();
                let total = if total <= 0.0 {
                    children.len() as f32
                } else {
                    total
                };
                let extent = match dir {
                    Dir::Row => rect.cols,
                    Dir::Col => rect.rows,
                };
                // Integer apportionment that sums exactly to `extent` (last child
                // absorbs the rounding remainder — no gaps, no overlap). Each
                // non-last child is clamped to the space still remaining so the
                // running offset can never exceed `extent`: with many tiny splits
                // the per-child `round()` can each round up, and an unclamped sum
                // would push later children past the rect boundary (and collapse
                // the last child to 0). Clamping keeps every child inside `rect`.
                let mut offset = 0usize;
                for (i, b) in children.iter().enumerate() {
                    let w = if total <= 0.0 { 1.0 } else { b.weight.max(0.0) };
                    let remaining = extent.saturating_sub(offset);
                    let size = if i + 1 == children.len() {
                        remaining
                    } else {
                        (((w / total) * extent as f32).round() as usize).min(remaining)
                    };
                    let child_rect = match dir {
                        Dir::Row => Rect {
                            x: rect.x + offset,
                            y: rect.y,
                            cols: size,
                            rows: rect.rows,
                        },
                        Dir::Col => Rect {
                            x: rect.x,
                            y: rect.y + offset,
                            cols: rect.cols,
                            rows: size,
                        },
                    };
                    b.child.layout_into(child_rect, out);
                    offset += size;
                }
            }
        }
    }

    /// Every pane id in the tree (visible or not) — for spawn/teardown bookkeeping.
    pub fn pane_ids(&self) -> Vec<PaneId> {
        let mut v = Vec::new();
        self.collect_ids(&mut v);
        v
    }

    fn collect_ids(&self, v: &mut Vec<PaneId>) {
        match self {
            CenterTree::Leaf(p) => v.push(*p),
            CenterTree::Stack { panes, .. } => v.extend_from_slice(panes),
            CenterTree::Split { children, .. } => {
                for b in children {
                    b.child.collect_ids(v);
                }
            }
        }
    }

    /// Rewrite every leaf id through `f` (used to remap a resurrected tree's
    /// stale pane ids onto freshly-spawned panes).
    pub fn remap(&mut self, f: &mut impl FnMut(PaneId) -> PaneId) {
        match self {
            CenterTree::Leaf(p) => *p = f(*p),
            CenterTree::Stack { panes, .. } => {
                for p in panes {
                    *p = f(*p);
                }
            }
            CenterTree::Split { children, .. } => {
                for b in children {
                    b.child.remap(f);
                }
            }
        }
    }

    /// Split the leaf `target` along `dir`, adding `new_id` beside it (equal
    /// weights). Returns whether the target was found.
    pub fn split(&mut self, target: PaneId, dir: Dir, new_id: PaneId) -> bool {
        match self {
            CenterTree::Leaf(p) if *p == target => {
                let old = *p;
                *self = CenterTree::Split {
                    dir,
                    children: vec![
                        Branch {
                            weight: 1.0,
                            child: CenterTree::Leaf(old),
                        },
                        Branch {
                            weight: 1.0,
                            child: CenterTree::Leaf(new_id),
                        },
                    ],
                };
                true
            }
            CenterTree::Leaf(_) | CenterTree::Stack { .. } => false,
            CenterTree::Split { children, .. } => children
                .iter_mut()
                .any(|b| b.child.split(target, dir, new_id)),
        }
    }

    /// Remove leaf `target` from a split, collapsing a now-single-child split
    /// into that child. Returns `true` if removed. Returns `false` when the tree
    /// is just `Leaf(target)` (the caller closes the whole tab instead).
    pub fn remove(&mut self, target: PaneId) -> bool {
        match self {
            CenterTree::Leaf(_) => false,
            CenterTree::Stack { panes, active } => {
                if let Some(i) = panes.iter().position(|p| *p == target) {
                    panes.remove(i);
                    if *active >= panes.len() {
                        *active = panes.len().saturating_sub(1);
                    }
                    !panes.is_empty()
                } else {
                    false
                }
            }
            CenterTree::Split { children, .. } => {
                // Direct child leaf == target?
                if let Some(i) = children
                    .iter()
                    .position(|b| matches!(&b.child, CenterTree::Leaf(p) if *p == target))
                {
                    children.remove(i);
                    if children.len() == 1 {
                        let only = children.pop().unwrap().child;
                        *self = only;
                    }
                    return true;
                }
                // Otherwise recurse into nested splits/stacks.
                for b in children.iter_mut() {
                    if b.child.remove(target) {
                        return true;
                    }
                }
                false
            }
        }
    }
}

/// Horizontal padding (cells) between a pane's frame ring and its content,
/// each side — `[theme] pane_padding`. A process-global because the framed
/// layout is computed from pure render paths everywhere; the loop stores it
/// at startup and on config reload.
pub static PANE_HPAD: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Inset a rect by the pane's frame ring (1 cell) plus the configured
/// horizontal padding. Degenerate rects collapse toward zero.
pub fn inset(r: Rect) -> Rect {
    let pad = PANE_HPAD.load(std::sync::atomic::Ordering::Relaxed);
    Rect {
        x: r.x + 1 + pad,
        y: r.y + 1,
        cols: r.cols.saturating_sub(2 + 2 * pad),
        rows: r.rows.saturating_sub(2),
    }
}

impl CenterTree {
    /// Like [`CenterTree::layout`], but every pane reserves a 1-cell frame ring:
    /// yields `(pane, frame rect, content rect)`. The frame is where
    /// `borders::draw_pane_frames` paints; the content rect is what the PTY and
    /// emulator surface get.
    pub fn layout_framed(&self, rect: Rect) -> Vec<(PaneId, Rect, Rect)> {
        self.layout(rect)
            .into_iter()
            .map(|(p, r)| (p, r, inset(r)))
            .collect()
    }
}

/// A focus-move direction. Reused by the resize/swap geometry ops so "the pane
/// to the left" means the same thing to focus, resize and swap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Move {
    Left,
    Right,
    Up,
    Down,
}

impl Move {
    /// The split axis this direction operates on: `Left`/`Right` shift a row
    /// split's columns, `Up`/`Down` a column split's rows.
    fn axis(self) -> Dir {
        match self {
            Move::Left | Move::Right => Dir::Row,
            Move::Up | Move::Down => Dir::Col,
        }
    }

    /// Whether the dir-side neighbour sits at the *higher* child index
    /// (`Right`/`Down`) rather than the lower (`Left`/`Up`).
    fn toward_high(self) -> bool {
        matches!(self, Move::Right | Move::Down)
    }
}

/// A side of a pane — the target edge for a drag-to-rearrange re-anchor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Left,
    Right,
    Top,
    Bottom,
}

/// Fraction of an adjacent pair's combined weight moved per keyboard resize
/// step. Small enough to feel like a nudge, large enough to be visible.
pub const RESIZE_STEP: f32 = 0.06;

/// Each of an adjacent pair keeps at least this fraction of their combined
/// weight — the clamp that makes "a pane can never be resized to zero" true at
/// the weight level (integer cell apportionment bounds the rest).
pub const MIN_SHARE: f32 = 0.10;

/// Outcome of [`CenterTree::resize`] — the caller repaints + persists on
/// `Resized`, shows a hint on `NoTarget`, and does nothing on `AtLimit`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResizeOutcome {
    /// Weights changed; persist the new layout and repaint.
    Resized,
    /// A matching border was found but the neighbour is already at the minimum
    /// share — a harmless no-op (repeated presses stop here).
    AtLimit,
    /// No matching-axis ancestor with a neighbour on that side — a harmless
    /// no-op with a statusbar hint.
    NoTarget,
}

/// Internal result of the recursive resize walk.
enum WalkResize {
    /// The target was not in this subtree.
    NotFound,
    /// The target is in this subtree but no resize happened here — bubble up so
    /// an ancestor split on the matching axis can host it.
    Contains,
    /// The resize was performed (or found its border and hit the clamp).
    Done { changed: bool },
}

impl CenterTree {
    /// Grow the focused pane by one `step` toward `mv`, shrinking the neighbour
    /// on that side. Operates at the nearest ancestor split whose axis matches
    /// `mv` and which has a neighbour on the `mv` side of the branch containing
    /// `target`; weight only ever moves between that adjacent pair, so the rest
    /// of the split (and the whole-tree total) is undisturbed. Clamped so the
    /// neighbour never drops below [`MIN_SHARE`].
    pub fn resize(&mut self, target: PaneId, mv: Move, step: f32) -> ResizeOutcome {
        match self.resize_walk(target, mv.axis(), mv.toward_high(), step) {
            WalkResize::Done { changed: true } => ResizeOutcome::Resized,
            WalkResize::Done { changed: false } => ResizeOutcome::AtLimit,
            WalkResize::NotFound | WalkResize::Contains => ResizeOutcome::NoTarget,
        }
    }

    fn resize_walk(
        &mut self,
        target: PaneId,
        axis: Dir,
        toward_high: bool,
        step: f32,
    ) -> WalkResize {
        match self {
            CenterTree::Leaf(p) => {
                if *p == target {
                    WalkResize::Contains
                } else {
                    WalkResize::NotFound
                }
            }
            // A stack is a positional unit: if the target is any member, the
            // resize happens at the stack's parent split.
            CenterTree::Stack { panes, .. } => {
                if panes.contains(&target) {
                    WalkResize::Contains
                } else {
                    WalkResize::NotFound
                }
            }
            CenterTree::Split { dir, children } => {
                let dir = *dir;
                for i in 0..children.len() {
                    match children[i]
                        .child
                        .resize_walk(target, axis, toward_high, step)
                    {
                        WalkResize::Done { changed } => return WalkResize::Done { changed },
                        WalkResize::NotFound => continue,
                        WalkResize::Contains => {
                            if dir == axis {
                                let neighbour = if toward_high {
                                    (i + 1 < children.len()).then_some(i + 1)
                                } else {
                                    (i >= 1).then_some(i - 1)
                                };
                                if let Some(j) = neighbour {
                                    let changed = shift_pair(children, i, j, step);
                                    return WalkResize::Done { changed };
                                }
                            }
                            // Wrong axis, or no neighbour on this side here:
                            // bubble up so a higher matching-axis ancestor tries.
                            return WalkResize::Contains;
                        }
                    }
                }
                WalkResize::NotFound
            }
        }
    }

    /// Exchange the positional slots of panes `a` and `b`, keeping each slot's
    /// weight (so a small pane swapped into a big slot becomes big — tmux
    /// `swap-pane` semantics). When a pane is a stack member the *whole stack*
    /// is the unit that moves (stacks are a positional unit). Returns whether
    /// the swap happened (both panes must exist and differ).
    pub fn swap(&mut self, a: PaneId, b: PaneId) -> bool {
        if a == b {
            return false;
        }
        if self.positional_node_mut(a).is_none() || self.positional_node_mut(b).is_none() {
            return false;
        }
        // A sentinel pane id no live tree ever mints (ids are small counters).
        const SENTINEL: PaneId = PaneId::MAX;
        // Lift a into a sentinel, drop it where b was, then drop b where the
        // sentinel is. Three fresh borrows sidestep two-&mut aliasing; the slots
        // are disjoint because two visible panes never share a positional node.
        let a_node = std::mem::replace(
            self.positional_node_mut(a).expect("a present"),
            CenterTree::Leaf(SENTINEL),
        );
        let b_node = std::mem::replace(self.positional_node_mut(b).expect("b present"), a_node);
        *self
            .positional_node_mut(SENTINEL)
            .expect("sentinel present") = b_node;
        true
    }

    /// Re-anchor `dragged` as a new split on `side` of `target` (the
    /// drag-to-edge rearrange). `dragged` is removed from its current slot, then
    /// `target`'s slot becomes a two-child split with `dragged` on `side`.
    /// Weights renormalize to equal. Returns whether it happened (both must
    /// exist and differ, and `dragged` must be removable — never the sole pane).
    pub fn anchor(&mut self, dragged: PaneId, target: PaneId, side: Side) -> bool {
        if dragged == target
            || self.positional_node_mut(dragged).is_none()
            || self.positional_node_mut(target).is_none()
        {
            return false;
        }
        // Lift the dragged pane out of its slot (collapsing single-child splits).
        if !self.remove(dragged) {
            return false; // it was the sole pane — nothing to re-anchor onto
        }
        // Re-find the target (removal may have collapsed its parent) and wrap it.
        let Some(node) = self.positional_node_mut(target) else {
            return false;
        };
        let (dir, dragged_first) = match side {
            Side::Left => (Dir::Row, true),
            Side::Right => (Dir::Row, false),
            Side::Top => (Dir::Col, true),
            Side::Bottom => (Dir::Col, false),
        };
        // A sentinel no live tree mints; overwritten immediately (no re-lookup).
        let target_sub = std::mem::replace(node, CenterTree::Leaf(PaneId::MAX));
        let dragged_leaf = CenterTree::Leaf(dragged);
        let (a, b) = if dragged_first {
            (dragged_leaf, target_sub)
        } else {
            (target_sub, dragged_leaf)
        };
        *node = CenterTree::Split {
            dir,
            children: vec![
                Branch {
                    weight: 1.0,
                    child: a,
                },
                Branch {
                    weight: 1.0,
                    child: b,
                },
            ],
        };
        true
    }

    /// The positional node whose *visible representative* is `p`: the `Leaf`
    /// node for a tiled pane, or the enclosing `Stack` for a stacked member.
    /// This is the subtree that [`CenterTree::swap`] moves as a unit.
    fn positional_node_mut(&mut self, p: PaneId) -> Option<&mut CenterTree> {
        match self {
            CenterTree::Leaf(id) if *id == p => Some(self),
            CenterTree::Leaf(_) => None,
            CenterTree::Stack { panes, .. } if panes.contains(&p) => Some(self),
            CenterTree::Stack { .. } => None,
            CenterTree::Split { children, .. } => children
                .iter_mut()
                .find_map(|b| b.child.positional_node_mut(p)),
        }
    }
}

/// Move `step` fraction of the combined weight of an adjacent pair from
/// `shrink` into `grow`, clamped so `shrink` keeps at least [`MIN_SHARE`] of the
/// pair. The pair's sum is conserved, so siblings and the whole-tree total are
/// untouched. Returns whether any weight actually moved.
fn shift_pair(children: &mut [Branch], grow: usize, shrink: usize, step: f32) -> bool {
    let a = children[grow].weight.max(0.0);
    let b = children[shrink].weight.max(0.0);
    let pair = a + b;
    if pair <= 0.0 {
        return false;
    }
    let min = MIN_SHARE * pair;
    // Never pull `shrink` below the minimum share; never negative.
    let delta = (step * pair).min(b - min).max(0.0);
    if delta <= f32::EPSILON {
        return false;
    }
    children[grow].weight = a + delta;
    children[shrink].weight = b - delta;
    true
}

/// Pick the pane to focus when moving `dir` from `from`, given a computed
/// layout. Chooses the nearest pane whose center lies in that direction
/// (primary-axis distance, with a half-weight cross-axis penalty for alignment).
/// Pure → unit-tested without a terminal.
pub fn neighbor(layout: &[(PaneId, Rect)], from: PaneId, dir: Move) -> Option<PaneId> {
    let cur = layout.iter().find(|(id, _)| *id == from)?.1;
    let cx = cur.x as i64 + cur.cols as i64 / 2;
    let cy = cur.y as i64 + cur.rows as i64 / 2;
    layout
        .iter()
        .filter(|(id, _)| *id != from)
        .filter_map(|(id, r)| {
            let rx = r.x as i64 + r.cols as i64 / 2;
            let ry = r.y as i64 + r.rows as i64 / 2;
            let in_dir = match dir {
                Move::Left => rx < cx,
                Move::Right => rx > cx,
                Move::Up => ry < cy,
                Move::Down => ry > cy,
            };
            if !in_dir {
                return None;
            }
            let dist = match dir {
                Move::Left | Move::Right => (rx - cx).abs() + (ry - cy).abs() / 2,
                Move::Up | Move::Down => (ry - cy).abs() + (rx - cx).abs() / 2,
            };
            Some((*id, dist))
        })
        .min_by_key(|(_, d)| *d)
        .map(|(id, _)| id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full() -> Rect {
        Rect {
            x: 0,
            y: 0,
            cols: 100,
            rows: 40,
        }
    }

    #[test]
    fn leaf_fills_the_rect() {
        let t = CenterTree::single(7);
        assert_eq!(t.layout(full()), vec![(7, full())]);
    }

    #[test]
    fn even_row_split_partitions_columns_without_gaps() {
        let t = CenterTree::Split {
            dir: Dir::Row,
            children: vec![
                Branch {
                    weight: 1.0,
                    child: CenterTree::Leaf(1),
                },
                Branch {
                    weight: 1.0,
                    child: CenterTree::Leaf(2),
                },
            ],
        };
        let l = t.layout(full());
        assert_eq!(
            l[0],
            (
                1,
                Rect {
                    x: 0,
                    y: 0,
                    cols: 50,
                    rows: 40
                }
            )
        );
        assert_eq!(
            l[1],
            (
                2,
                Rect {
                    x: 50,
                    y: 0,
                    cols: 50,
                    rows: 40
                }
            )
        );
        // No gaps / overlap: columns sum to the full width.
        assert_eq!(l[0].1.cols + l[1].1.cols, 100);
    }

    #[test]
    fn weighted_col_split_apportions_rows_and_absorbs_remainder() {
        let t = CenterTree::Split {
            dir: Dir::Col,
            children: vec![
                Branch {
                    weight: 2.0,
                    child: CenterTree::Leaf(1),
                },
                Branch {
                    weight: 1.0,
                    child: CenterTree::Leaf(2),
                },
            ],
        };
        let l = t.layout(full());
        // 2:1 of 40 rows -> 27 (rounded) + remainder 13.
        assert_eq!(l[0].1.rows + l[1].1.rows, 40);
        assert!(l[0].1.rows > l[1].1.rows);
        assert_eq!(l[1].1.y, l[0].1.rows); // second starts where first ends
    }

    #[test]
    fn many_tiny_splits_stay_within_the_rect() {
        // Regression: with more children than the extent has cells, each
        // non-last child's `round()` can push the running offset past `extent`,
        // drawing children outside the parent rect (and collapsing the last to
        // 0). Clamping each child to the remaining space must keep every rect
        // inside the parent and the offsets monotonic and bounded.
        let parent = Rect {
            x: 0,
            y: 0,
            cols: 10,
            rows: 3,
        };
        let children: Vec<Branch> = (0..6)
            .map(|i| Branch {
                weight: 1.0,
                child: CenterTree::Leaf(i as PaneId),
            })
            .collect();
        let t = CenterTree::Split {
            dir: Dir::Col,
            children,
        };
        let l = t.layout(parent);
        for (_, r) in &l {
            assert!(r.y >= parent.y, "child starts before the rect: {r:?}");
            assert!(
                r.y + r.rows <= parent.y + parent.rows,
                "child extends past the rect boundary: {r:?}"
            );
            assert!(
                r.x + r.cols <= parent.x + parent.cols,
                "child extends past the rect width: {r:?}"
            );
        }
        // Offsets never regress and the visible children exactly tile the extent.
        let mut expected_y = parent.y;
        for (_, r) in &l {
            assert_eq!(r.y, expected_y, "gap or overlap between children: {r:?}");
            expected_y += r.rows;
        }
        assert_eq!(expected_y, parent.y + parent.rows);
    }

    #[test]
    fn stack_shows_only_the_active_pane() {
        let t = CenterTree::Stack {
            panes: vec![10, 11, 12],
            active: 1,
        };
        let l = t.layout(full());
        assert_eq!(l, vec![(11, full())]);
        assert_eq!(t.pane_ids(), vec![10, 11, 12]);
    }

    #[test]
    fn split_wraps_the_target_leaf() {
        let mut t = CenterTree::single(1);
        assert!(t.split(1, Dir::Row, 2));
        // 1 and 2 side by side.
        let l = t.layout(full());
        assert_eq!(l.len(), 2);
        assert_eq!(l[0].0, 1);
        assert_eq!(l[1].0, 2);
        // Split again on pane 2, vertically.
        assert!(t.split(2, Dir::Col, 3));
        assert_eq!(t.pane_ids(), vec![1, 2, 3]);
        // Splitting a missing pane is a no-op.
        assert!(!t.split(99, Dir::Row, 4));
    }

    #[test]
    fn remap_rewrites_leaf_ids() {
        let mut t = CenterTree::Split {
            dir: Dir::Row,
            children: vec![
                Branch {
                    weight: 1.0,
                    child: CenterTree::Leaf(5),
                },
                Branch {
                    weight: 1.0,
                    child: CenterTree::Leaf(6),
                },
            ],
        };
        let mut next = 100;
        t.remap(&mut |_| {
            next += 1;
            next
        });
        assert_eq!(t.pane_ids(), vec![101, 102]);
    }

    #[test]
    fn remove_collapses_single_child_splits() {
        let mut t = CenterTree::single(1);
        t.split(1, Dir::Row, 2);
        t.split(2, Dir::Col, 3); // 1 | (2 / 3)
        assert_eq!(t.pane_ids(), vec![1, 2, 3]);

        // Remove 3 -> the (2/3) split collapses to Leaf(2): 1 | 2.
        assert!(t.remove(3));
        assert_eq!(t.pane_ids(), vec![1, 2]);
        // Remove 2 -> the root split collapses to Leaf(1).
        assert!(t.remove(2));
        assert_eq!(t, CenterTree::Leaf(1));
        // Removing the sole leaf returns false (caller closes the tab).
        assert!(!t.remove(1));
    }

    #[test]
    fn neighbor_navigates_geometrically() {
        // 1 | 2 side by side; 2 split into 2 (top) / 3 (bottom).
        let mut t = CenterTree::single(1);
        t.split(1, Dir::Row, 2);
        t.split(2, Dir::Col, 3);
        let l = t.layout(full());
        // From 1, Right reaches the right column (its top pane, 2).
        assert_eq!(neighbor(&l, 1, Move::Right), Some(2));
        // From 2 (top-right), Down reaches 3 (bottom-right).
        assert_eq!(neighbor(&l, 2, Move::Down), Some(3));
        // From 3, Left reaches the left column (1).
        assert_eq!(neighbor(&l, 3, Move::Left), Some(1));
        // Nothing to the left of 1.
        assert_eq!(neighbor(&l, 1, Move::Left), None);
    }

    // Weight of the branch whose subtree contains `pane`, at the given split.
    fn weight_of(children: &[Branch], idx: usize) -> f32 {
        children[idx].weight
    }

    #[test]
    fn resize_grows_the_focused_pane_toward_the_direction() {
        // 1 | 2 side by side (a Row split), even weights.
        let mut t = CenterTree::single(1);
        t.split(1, Dir::Row, 2);
        assert_eq!(
            t.resize(1, Move::Right, RESIZE_STEP),
            ResizeOutcome::Resized
        );
        if let CenterTree::Split { children, .. } = &t {
            // Focused (child 0) grew, neighbour (child 1) shrank; sum conserved.
            assert!(weight_of(children, 0) > weight_of(children, 1));
            assert!((weight_of(children, 0) + weight_of(children, 1) - 2.0).abs() < 1e-4);
        } else {
            panic!("expected split");
        }
    }

    #[test]
    fn resize_cannot_eliminate_a_pane() {
        let mut t = CenterTree::single(1);
        t.split(1, Dir::Row, 2);
        // Hammer resize-right until it stops moving.
        let mut last = ResizeOutcome::Resized;
        for _ in 0..1000 {
            last = t.resize(1, Move::Right, RESIZE_STEP);
            if last == ResizeOutcome::AtLimit {
                break;
            }
        }
        assert_eq!(last, ResizeOutcome::AtLimit);
        if let CenterTree::Split { children, .. } = &t {
            let pair = weight_of(children, 0) + weight_of(children, 1);
            // The neighbour never dropped below the minimum share.
            assert!(weight_of(children, 1) >= MIN_SHARE * pair - 1e-4);
            assert!(weight_of(children, 1) > 0.0);
        } else {
            panic!("expected split");
        }
    }

    #[test]
    fn resize_single_pane_and_wrong_axis_are_no_ops() {
        // A single pane: nothing to resize, any direction.
        let mut t = CenterTree::single(7);
        for mv in [Move::Left, Move::Right, Move::Up, Move::Down] {
            assert_eq!(t.resize(7, mv, RESIZE_STEP), ResizeOutcome::NoTarget);
        }
        // A row split has no vertical neighbour: resize-up/down is a no-op, but
        // resize-left/right work.
        let mut t = CenterTree::single(1);
        t.split(1, Dir::Row, 2);
        assert_eq!(t.resize(1, Move::Up, RESIZE_STEP), ResizeOutcome::NoTarget);
        assert_eq!(
            t.resize(1, Move::Down, RESIZE_STEP),
            ResizeOutcome::NoTarget
        );
        assert_eq!(
            t.resize(1, Move::Left, RESIZE_STEP),
            ResizeOutcome::NoTarget
        ); // 1 is leftmost
        assert_eq!(
            t.resize(1, Move::Right, RESIZE_STEP),
            ResizeOutcome::Resized
        );
    }

    #[test]
    fn resize_bubbles_up_to_the_matching_axis_ancestor() {
        // Row[ Leaf(1), Col[ Leaf(2), Leaf(3) ] ] — pane 2 is top of the right column.
        let mut t = CenterTree::single(1);
        t.split(1, Dir::Row, 2);
        t.split(2, Dir::Col, 3);
        // resize-left from 2: no Row neighbour inside the inner Col; bubbles up
        // to the root Row and shrinks the left pane (1), growing the column.
        assert_eq!(t.resize(2, Move::Left, RESIZE_STEP), ResizeOutcome::Resized);
        if let CenterTree::Split { children, dir } = &t {
            assert_eq!(*dir, Dir::Row);
            assert!(weight_of(children, 1) > weight_of(children, 0)); // column grew
        } else {
            panic!("expected split");
        }
        // resize-right from 2: the column is the rightmost child — nothing to eat.
        assert_eq!(
            t.resize(2, Move::Right, RESIZE_STEP),
            ResizeOutcome::NoTarget
        );
        // resize-down from 2: the inner Col has 3 below it.
        assert_eq!(t.resize(2, Move::Down, RESIZE_STEP), ResizeOutcome::Resized);
    }

    #[test]
    fn resize_targets_a_stacks_branch() {
        // Row[ Leaf(1), Stack{[2,3], active 0} ].
        let mut t = CenterTree::Split {
            dir: Dir::Row,
            children: vec![
                Branch {
                    weight: 1.0,
                    child: CenterTree::Leaf(1),
                },
                Branch {
                    weight: 1.0,
                    child: CenterTree::Stack {
                        panes: vec![2, 3],
                        active: 0,
                    },
                },
            ],
        };
        // Resizing from the visible stack member grows the stack's whole branch.
        assert_eq!(t.resize(2, Move::Left, RESIZE_STEP), ResizeOutcome::Resized);
        if let CenterTree::Split { children, .. } = &t {
            assert!(weight_of(children, 1) > weight_of(children, 0));
        } else {
            panic!("expected split");
        }
    }

    #[test]
    fn swap_exchanges_positions_keeping_slot_weights() {
        // 1 | 2 with a 70/30 split.
        let mut t = CenterTree::Split {
            dir: Dir::Row,
            children: vec![
                Branch {
                    weight: 0.7,
                    child: CenterTree::Leaf(1),
                },
                Branch {
                    weight: 0.3,
                    child: CenterTree::Leaf(2),
                },
            ],
        };
        assert!(t.swap(1, 2));
        // Positions exchanged; each pane adopts the OTHER slot's weight.
        if let CenterTree::Split { children, .. } = &t {
            assert_eq!(children[0].child, CenterTree::Leaf(2));
            assert_eq!(children[1].child, CenterTree::Leaf(1));
            assert!((weight_of(children, 0) - 0.7).abs() < 1e-6); // wide slot kept its weight
            assert!((weight_of(children, 1) - 0.3).abs() < 1e-6);
        } else {
            panic!("expected split");
        }
        // Swapping with self or a missing pane is a no-op.
        assert!(!t.swap(1, 1));
        assert!(!t.swap(1, 99));
    }

    #[test]
    fn swap_agrees_with_focus_neighbour() {
        // For any pair the geometry walk resolves, swapping toward that direction
        // exchanges exactly the pane focus would land on.
        let mut t = CenterTree::single(1);
        t.split(1, Dir::Row, 2);
        t.split(2, Dir::Col, 3); // 1 | (2 / 3)
        let l = t.layout(full());
        // focus-down from 2 lands on 3.
        let n = neighbor(&l, 2, Move::Down).unwrap();
        assert_eq!(n, 3);
        assert!(t.swap(2, n));
        // 2 and 3 exchanged their slots.
        assert_eq!(t.pane_ids(), vec![1, 3, 2]);
    }

    #[test]
    fn anchor_reanchors_a_pane_on_an_edge() {
        // 1 | 2 side by side. Drag 1 onto the bottom edge of 2 → 2's slot becomes
        // a column split with 2 above 1.
        let mut t = CenterTree::single(1);
        t.split(1, Dir::Row, 2);
        assert!(t.anchor(1, 2, Side::Bottom));
        // The whole tree is now Col[ 2, 1 ] (1 was the only other pane, so the
        // root collapsed to Leaf(2) then re-split).
        match &t {
            CenterTree::Split { dir, children } => {
                assert_eq!(*dir, Dir::Col);
                assert_eq!(children[0].child, CenterTree::Leaf(2));
                assert_eq!(children[1].child, CenterTree::Leaf(1));
            }
            other => panic!("expected a column split, got {other:?}"),
        }
    }

    #[test]
    fn anchor_left_and_right_choose_row_order() {
        // 1 | 2 | 3 in a row. Drag 3 onto the left edge of 1.
        let mut t = CenterTree::Split {
            dir: Dir::Row,
            children: vec![
                Branch {
                    weight: 1.0,
                    child: CenterTree::Leaf(1),
                },
                Branch {
                    weight: 1.0,
                    child: CenterTree::Leaf(2),
                },
                Branch {
                    weight: 1.0,
                    child: CenterTree::Leaf(3),
                },
            ],
        };
        assert!(t.anchor(3, 1, Side::Left));
        // 1's slot becomes Row[3, 1]; the outer row is now [ [3,1], 2 ].
        let ids = t.pane_ids();
        assert_eq!(ids, vec![3, 1, 2]);
    }

    #[test]
    fn anchor_rejects_self_and_missing_and_sole_pane() {
        let mut t = CenterTree::single(1);
        t.split(1, Dir::Row, 2);
        assert!(!t.anchor(1, 1, Side::Left)); // onto itself
        assert!(!t.anchor(1, 99, Side::Left)); // missing target
        // Sole pane can't be lifted.
        let mut single = CenterTree::single(5);
        assert!(!single.anchor(5, 5, Side::Top));
    }

    #[test]
    fn swap_moves_a_whole_stack_as_a_unit() {
        // Row[ Leaf(1), Stack{[2,3], active 0} ] — swap the leaf with the stack.
        let mut t = CenterTree::Split {
            dir: Dir::Row,
            children: vec![
                Branch {
                    weight: 0.4,
                    child: CenterTree::Leaf(1),
                },
                Branch {
                    weight: 0.6,
                    child: CenterTree::Stack {
                        panes: vec![2, 3],
                        active: 0,
                    },
                },
            ],
        };
        assert!(t.swap(1, 2)); // 2 is the visible stack member
        if let CenterTree::Split { children, .. } = &t {
            // The whole stack moved to slot 0 (keeping slot 0's weight), the leaf to slot 1.
            assert!(matches!(children[0].child, CenterTree::Stack { .. }));
            assert_eq!(children[1].child, CenterTree::Leaf(1));
            assert!((weight_of(children, 0) - 0.4).abs() < 1e-6);
        } else {
            panic!("expected split");
        }
    }

    #[test]
    fn serde_roundtrip_preserves_the_tree() {
        let t = CenterTree::Split {
            dir: Dir::Row,
            children: vec![
                Branch {
                    weight: 1.0,
                    child: CenterTree::Leaf(1),
                },
                Branch {
                    weight: 2.0,
                    child: CenterTree::Stack {
                        panes: vec![2, 3],
                        active: 0,
                    },
                },
            ],
        };
        let json = serde_json::to_string(&t).unwrap();
        let back: CenterTree = serde_json::from_str(&json).unwrap();
        assert_eq!(t, back);
    }
}
