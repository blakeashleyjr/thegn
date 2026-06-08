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
                // absorbs the rounding remainder — no gaps, no overlap).
                let mut offset = 0usize;
                for (i, b) in children.iter().enumerate() {
                    let w = if total <= 0.0 { 1.0 } else { b.weight.max(0.0) };
                    let size = if i + 1 == children.len() {
                        extent.saturating_sub(offset)
                    } else {
                        ((w / total) * extent as f32).round() as usize
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

/// A focus-move direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Move {
    Left,
    Right,
    Up,
    Down,
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
