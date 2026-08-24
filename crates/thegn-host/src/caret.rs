//! Who owns the hardware cursor this frame — recorded by the code that paints,
//! not by a list someone has to remember to update.
//!
//! The terminal caret is an out-of-band channel: the compositor paints cells
//! into a `Surface`, then separately tells the outer terminal where to park its
//! cursor. Those two decisions used to be made in different places, so every
//! popup added to the render stack had to *also* be added to a hand-written
//! `fullscreen_modal` predicate in the loop or the focused pane's caret would
//! keep blinking on top of it. It drifted, of course: help, the PR/diff views,
//! the monitor, bar-detail popups, the media overlay and the corner pin card all
//! punched a caret through themselves.
//!
//! So the decision is derived from geometry instead of membership. Two facts are
//! recorded during compose, each by the exact call that draws the thing:
//!
//! - a **cover** — [`cover`], called from [`crate::layer::open_layer`], the
//!   chokepoint every boxed popup already goes through. A new popup is therefore
//!   accounted for the moment it renders, with nothing to maintain here.
//! - a **claim** — [`claim`], recorded by [`crate::seg`] when it emits a caret
//!   seg ([`crate::seg::caret`]). A text field that draws its own `▏` says "the
//!   real cursor belongs *here*", and the cell falls out of the actual line
//!   layout rather than a second copy of the geometry math.
//!
//! [`resolve`] then arbitrates, and is pure so the rules are unit-tested rather
//! than asserted. This is strictly better than the boolean it replaces: a toast
//! that floats clear of the caret leaves it alone, while one that covers it
//! hides it — correct in both cases, with nothing enumerated.
//!
//! # Frame lifetime
//!
//! [`begin_frame`] must be called only at the top of a **full** compose, never
//! on the incremental pane-only path. Incremental frames deliberately skip the
//! overlay stack (see `render_plan`), so they inherit the previous full frame's
//! covers and claim. That is sound because any change to the overlay set marks
//! chrome dirty, which forces a full frame.
//!
//! State is thread-local: compose runs on the loop thread, and a `thread_local!`
//! keeps parallel `cargo test` runs from racing each other (`caps.rs` had to
//! grow a `test_override` module precisely because its holder is process-wide).

use crate::compositor::Rect;
use std::cell::RefCell;

#[derive(Default)]
struct FrameCaret {
    /// Boxes painted over the band this frame, in draw order.
    covers: Vec<Rect>,
    /// The topmost input field asking for the real cursor. Overlays render in
    /// z-order, so the last claim wins.
    claim: Option<(usize, usize)>,
}

thread_local! {
    static FRAME: RefCell<FrameCaret> = RefCell::new(FrameCaret::default());
}

/// Start a new full frame: drop the previous frame's covers and claim.
///
/// Full compose only — see the module docs on frame lifetime.
pub fn begin_frame() {
    FRAME.with(|f| {
        let mut f = f.borrow_mut();
        f.covers.clear();
        f.claim = None;
    });
}

/// Record that `rect` is painted over the band this frame, so the pane caret
/// must not show through it.
pub fn cover(rect: Rect) {
    FRAME.with(|f| f.borrow_mut().covers.push(rect));
}

/// Record that an input field wants the real cursor at `(x, y)`.
pub fn claim(x: usize, y: usize) {
    FRAME.with(|f| f.borrow_mut().claim = Some((x, y)));
}

/// True when nothing painted over the band this frame — the bit the render plan
/// uses to decide whether a pane-only frame would corrupt an overlay.
pub fn no_covers() -> bool {
    FRAME.with(|f| f.borrow().covers.is_empty())
}

/// Resolve this frame's caret against a pane caret, using the recorded state.
pub fn resolve_frame(pane: Option<(usize, usize)>) -> Option<(usize, usize)> {
    FRAME.with(|f| {
        let f = f.borrow();
        resolve(f.claim, &f.covers, pane)
    })
}

/// Where the hardware cursor goes, or `None` to hide it.
///
/// An explicit `claim` wins outright — a focused text field owns the cursor even
/// though it sits inside its own popup's cover. Otherwise the focused pane's
/// caret survives, unless some popup covers the cell it would land on.
pub fn resolve(
    claim: Option<(usize, usize)>,
    covers: &[Rect],
    pane: Option<(usize, usize)>,
) -> Option<(usize, usize)> {
    if let Some(c) = claim {
        return Some(c);
    }
    let (x, y) = pane?;
    if covers.iter().any(|r| contains(r, x, y)) {
        return None;
    }
    Some((x, y))
}

fn contains(r: &Rect, x: usize, y: usize) -> bool {
    x >= r.x && x < r.x + r.cols && y >= r.y && y < r.y + r.rows
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: usize, y: usize, cols: usize, rows: usize) -> Rect {
        Rect { x, y, cols, rows }
    }

    #[test]
    fn no_overlay_keeps_the_pane_caret() {
        assert_eq!(resolve(None, &[], Some((4, 9))), Some((4, 9)));
    }

    #[test]
    fn a_cover_over_the_caret_hides_it() {
        // The reported bug: the help box sits over the focused pane's cursor.
        let help = rect(10, 5, 60, 20);
        assert_eq!(resolve(None, &[help], Some((30, 12))), None);
    }

    #[test]
    fn a_cover_clear_of_the_caret_leaves_it_alone() {
        // Why this beats a boolean: a corner toast must not blank the cursor
        // you are typing at.
        let toast = rect(60, 0, 20, 3);
        assert_eq!(resolve(None, &[toast], Some((4, 9))), Some((4, 9)));
    }

    #[test]
    fn cover_edges_are_half_open() {
        let r = rect(10, 5, 4, 2); // cols 10..14, rows 5..7
        assert_eq!(resolve(None, &[r], Some((10, 5))), None, "top-left is in");
        assert_eq!(
            resolve(None, &[r], Some((13, 6))),
            None,
            "bottom-right is in"
        );
        assert_eq!(
            resolve(None, &[r], Some((14, 6))),
            Some((14, 6)),
            "x past the right edge is out"
        );
        assert_eq!(
            resolve(None, &[r], Some((13, 7))),
            Some((13, 7)),
            "y past the bottom edge is out"
        );
        assert_eq!(
            resolve(None, &[r], Some((9, 5))),
            Some((9, 5)),
            "x before the left edge is out"
        );
        assert_eq!(
            resolve(None, &[r], Some((10, 4))),
            Some((10, 4)),
            "y above the top edge is out"
        );
    }

    #[test]
    fn any_of_several_covers_hides() {
        let a = rect(0, 0, 10, 10);
        let b = rect(40, 10, 10, 10);
        assert_eq!(resolve(None, &[a, b], Some((44, 14))), None);
        assert_eq!(resolve(None, &[a, b], Some((20, 14))), Some((20, 14)));
    }

    #[test]
    fn a_claim_wins_over_its_own_cover() {
        // A text field's caret sits *inside* the popup covering the band; the
        // claim is what distinguishes it from a pane caret bleeding through.
        let popup = rect(10, 5, 60, 20);
        assert_eq!(
            resolve(Some((14, 6)), &[popup], Some((30, 12))),
            Some((14, 6))
        );
    }

    #[test]
    fn a_claim_wins_with_no_pane_caret_at_all() {
        // Launch splash / corner focus: no pane caret, but a field still owns it.
        assert_eq!(resolve(Some((14, 6)), &[], None), Some((14, 6)));
    }

    #[test]
    fn no_pane_caret_and_no_claim_hides() {
        assert_eq!(resolve(None, &[], None), None);
    }

    #[test]
    fn holder_records_and_clears() {
        begin_frame();
        assert!(no_covers());
        assert_eq!(resolve_frame(Some((30, 12))), Some((30, 12)));

        cover(rect(10, 5, 60, 20));
        assert!(!no_covers());
        assert_eq!(resolve_frame(Some((30, 12))), None);

        claim(14, 6);
        assert_eq!(resolve_frame(Some((30, 12))), Some((14, 6)));

        // The topmost claim wins — overlays render in z-order.
        claim(20, 7);
        assert_eq!(resolve_frame(Some((30, 12))), Some((20, 7)));

        begin_frame();
        assert!(no_covers());
        assert_eq!(resolve_frame(Some((30, 12))), Some((30, 12)));
    }
}
