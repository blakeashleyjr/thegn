//! Pure geometry for center-pane **mouse** gestures — border-drag resize and
//! drag-to-rearrange. Both resolve a pointer plus the laid-out pane rects to an
//! *intent*, with no I/O and no tree mutation, so they are exhaustively unit
//! tested here; the run loop applies the resolved intent through the same
//! [`crate::center`] mutations the keyboard uses (so every mouse op has a
//! keyboard equivalent). Chrome owns the pane *frame* cells; a pointer inside a
//! pane's content rect is never a gesture here (it forwards to the pane app).

use crate::center::{PaneId, Side};
use crate::compositor::Rect;

/// Fraction of a target pane occupied by each edge band; the center is a swap.
const EDGE_BAND: f32 = 0.25;

/// Whether `(mx, my)` lies inside `r` (half-open on the high edges).
fn contains(r: Rect, mx: usize, my: usize) -> bool {
    mx >= r.x && mx < r.x + r.cols && my >= r.y && my < r.y + r.rows
}

/// A pane border under the pointer: the two adjacent panes and the drag axis.
/// `low` is the pane on the low side (left of a vertical border, above a
/// horizontal one); `high` is the other. Dragging toward `high` grows `low`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BorderHit {
    pub low: PaneId,
    pub high: PaneId,
    /// A vertical border (drag left/right along columns) vs horizontal.
    pub vertical: bool,
}

/// Hit-test a pointer against the shared borders between panes. `frames` is the
/// laid-out `(pane, frame rect, content rect)` list. Returns the border the
/// pointer is grabbing, or `None` when the pointer is inside a pane's content
/// (that forwards to the app) or not on any seam.
pub fn border_at(frames: &[(PaneId, Rect, Rect)], mx: usize, my: usize) -> Option<BorderHit> {
    // Inside a content rect ⇒ pane input, never a chrome border.
    if frames.iter().any(|(_, _, c)| contains(*c, mx, my)) {
        return None;
    }
    // Vertical seam: L's right frame edge meets R's left frame edge (frames abut
    // with no gap), the pointer sits on either border column, and the rows
    // overlap. Scan all ordered pairs; `layout` tiles without gaps so exactly
    // one pair matches a given seam cell.
    for (lid, lf, _) in frames {
        for (rid, rf, _) in frames {
            if lid == rid {
                continue;
            }
            let l_right = lf.x + lf.cols; // exclusive
            let on_seam = l_right == rf.x && (mx + 1 == rf.x || mx == rf.x);
            let rows_overlap = my >= lf.y.max(rf.y) && my < (lf.y + lf.rows).min(rf.y + rf.rows);
            if on_seam && rows_overlap {
                return Some(BorderHit {
                    low: *lid,
                    high: *rid,
                    vertical: true,
                });
            }
        }
    }
    // Horizontal seam: L's bottom edge meets R's top edge; columns overlap.
    for (lid, lf, _) in frames {
        for (rid, rf, _) in frames {
            if lid == rid {
                continue;
            }
            let l_bottom = lf.y + lf.rows;
            let on_seam = l_bottom == rf.y && (my + 1 == rf.y || my == rf.y);
            let cols_overlap = mx >= lf.x.max(rf.x) && mx < (lf.x + lf.cols).min(rf.x + rf.cols);
            if on_seam && cols_overlap {
                return Some(BorderHit {
                    low: *lid,
                    high: *rid,
                    vertical: false,
                });
            }
        }
    }
    None
}

/// Render-facing drag-rearrange feedback carried on the frame model: the pane a
/// drop would land on, and whether it swaps or re-anchors on a side. The run
/// loop derives it from [`resolve_drop`] each motion sample; `render_panes`
/// paints the highlight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaneDropViz {
    pub target: PaneId,
    pub kind: DropKind,
}

/// The outcome a drop highlight previews.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropKind {
    /// Swap with the target pane (highlight its whole frame).
    Swap,
    /// Re-anchor on a side (highlight that edge band).
    Anchor(Side),
}

impl DropTarget {
    /// The render-facing viz for this drop outcome, or `None` for no target.
    pub fn viz(self) -> Option<PaneDropViz> {
        match self {
            DropTarget::Swap(target) => Some(PaneDropViz {
                target,
                kind: DropKind::Swap,
            }),
            DropTarget::Anchor(target, side) => Some(PaneDropViz {
                target,
                kind: DropKind::Anchor(side),
            }),
            DropTarget::None => None,
        }
    }
}

/// Where a lifted pane would drop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropTarget {
    /// Swap the dragged pane with `target` (dropped on its center).
    Swap(PaneId),
    /// Re-anchor the dragged pane as a new split on `side` of `target`.
    Anchor(PaneId, Side),
    /// No valid target (over the dragged pane itself, or outside all panes).
    None,
}

/// Resolve where the lifted `dragged` pane would drop given the pointer. The
/// target is the pane whose *content* rect holds the pointer; its center region
/// is a swap, an outer edge band (~a quarter) re-anchors on that side. Dropping
/// on the dragged pane itself, or outside every pane, is `None`. Pure fn of the
/// pointer and the rects — unit tested like a truth table.
pub fn resolve_drop(
    frames: &[(PaneId, Rect, Rect)],
    dragged: PaneId,
    mx: usize,
    my: usize,
) -> DropTarget {
    let Some((tid, _, c)) = frames
        .iter()
        .copied()
        .find(|(_, _, c)| contains(*c, mx, my))
    else {
        return DropTarget::None;
    };
    if tid == dragged {
        return DropTarget::None;
    }
    // Position within the target content rect, as fractions in 0..1.
    let rel_x = (mx - c.x) as f32 / c.cols.max(1) as f32;
    let rel_y = (my - c.y) as f32 / c.rows.max(1) as f32;
    // Distance to each edge; the nearest edge wins if it's inside the band.
    let edges = [
        (rel_x, Side::Left),
        (1.0 - rel_x, Side::Right),
        (rel_y, Side::Top),
        (1.0 - rel_y, Side::Bottom),
    ];
    let (min_d, side) = edges
        .into_iter()
        .fold((f32::MAX, Side::Left), |(md, ms), (d, s)| {
            if d < md { (d, s) } else { (md, ms) }
        });
    if min_d < EDGE_BAND {
        DropTarget::Anchor(tid, side)
    } else {
        DropTarget::Swap(tid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::center::{CenterTree, Dir};

    // A 1|2 row split over a 100x40 rect, framed.
    fn side_by_side() -> Vec<(PaneId, Rect, Rect)> {
        let mut t = CenterTree::single(1);
        t.split(1, Dir::Row, 2);
        t.layout_framed(Rect {
            x: 0,
            y: 0,
            cols: 100,
            rows: 40,
        })
    }

    // A 1/2 column split (stacked) over 100x40.
    fn stacked() -> Vec<(PaneId, Rect, Rect)> {
        let mut t = CenterTree::single(1);
        t.split(1, Dir::Col, 2);
        t.layout_framed(Rect {
            x: 0,
            y: 0,
            cols: 100,
            rows: 40,
        })
    }

    #[test]
    fn border_at_finds_the_vertical_seam() {
        let frames = side_by_side();
        // The seam is at column 50 (pane 2's frame left / pane 1's frame right).
        let hit = border_at(&frames, 50, 20).expect("on the seam");
        assert_eq!((hit.low, hit.high, hit.vertical), (1, 2, true));
        // One cell left (pane 1's right border) also grabs it.
        assert!(border_at(&frames, 49, 20).is_some());
    }

    #[test]
    fn border_at_ignores_content_clicks() {
        let frames = side_by_side();
        // Deep inside pane 1's content — must forward to the app, not grab.
        assert!(border_at(&frames, 20, 20).is_none());
        // Inside pane 2's content.
        assert!(border_at(&frames, 80, 20).is_none());
    }

    #[test]
    fn border_at_finds_the_horizontal_seam() {
        let frames = stacked();
        // Seam at row 20.
        let hit = border_at(&frames, 40, 20).expect("on the horizontal seam");
        assert_eq!((hit.low, hit.high, hit.vertical), (1, 2, false));
    }

    #[test]
    fn drop_on_center_is_a_swap() {
        let frames = side_by_side();
        // Dragging pane 1 onto the center of pane 2.
        assert_eq!(resolve_drop(&frames, 1, 75, 20), DropTarget::Swap(2));
    }

    #[test]
    fn drop_on_an_edge_band_anchors_on_that_side() {
        let frames = side_by_side();
        // Pane 2's content spans ~x 51..99. Its bottom edge band re-anchors below.
        assert_eq!(
            resolve_drop(&frames, 1, 75, 39),
            DropTarget::Anchor(2, Side::Bottom)
        );
        // Its left edge band re-anchors to the left of pane 2.
        assert_eq!(
            resolve_drop(&frames, 1, 52, 20),
            DropTarget::Anchor(2, Side::Left)
        );
    }

    #[test]
    fn drop_on_self_or_outside_is_none() {
        let frames = side_by_side();
        // Onto pane 1's own content while dragging pane 1.
        assert_eq!(resolve_drop(&frames, 1, 20, 20), DropTarget::None);
        // Outside every pane.
        assert_eq!(resolve_drop(&frames, 1, 500, 500), DropTarget::None);
    }
}
