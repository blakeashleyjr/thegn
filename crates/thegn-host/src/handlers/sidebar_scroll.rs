//! Sidebar scroll behavior: the state-layer half of the scroll model.
//!
//! `sidebar_view::build_sidebar` is a pure function of `(model, rect,
//! desired_scroll)` — paint and the mouse hit-test both call it, so it must
//! stay a geometric mapping and cannot decide *where* the window should be.
//! That policy lives here:
//!
//! - [`SidebarState::scroll_by`] moves the VIEWPORT (wheel, drag autoscroll).
//! - [`SidebarState::reanchor_cursor`] moves the CURSOR back into a
//!   wheel-scrolled window, so an action key never targets a row you can't see.
//! - [`SidebarState::settle_scroll`] is the per-frame reconciliation, and the
//!   single place the viewport follows the cursor (keyboard navigation).
//!
//! Before this split, `clamp_sidebar_scroll` derived the window from the cursor
//! (`desired.min(cursor)`) with no upper bound, so the list could never be
//! scrolled to its end and every unfocused `rebuild` — which re-snaps the
//! cursor to the active worktree (`run.rs`) — yanked the window back to the top
//! and silently dropped the tail rows.

use crate::chrome::FrameModel;
use crate::compositor::Rect;
use crate::handlers::sidebar_persist::SidebarState;
use crate::sidebar_view::{max_sidebar_scroll, scroll_to_reveal, sidebar_geom, sidebar_window};

impl SidebarState {
    /// Move the viewport by `delta` visible rows, clamped to
    /// `[0, max_sidebar_scroll]`. **The cursor does not move** — pair this with
    /// [`Self::reanchor_cursor`] before any cursor-relative action.
    ///
    /// Returns whether the window actually moved, so callers can gate `dirty`
    /// (a wheel tick at the end of the list must not force a repaint).
    pub(crate) fn scroll_by(&mut self, model: &mut FrameModel, rect: Rect, delta: isize) -> bool {
        let geom = sidebar_geom(model, rect);
        let max = max_sidebar_scroll(&geom.heights, geom.list_rows);
        let next = self.scroll.saturating_add_signed(delta).min(max);
        let moved = next != self.scroll;
        self.scroll = next;
        self.stamp_window(&geom);
        self.sync(model);
        moved
    }

    /// If the cursor sits outside the cached window, snap it to the nearest
    /// in-window row. O(1) — reads the stamped `first_visible`/`last_visible`,
    /// no layout pass — so it is cheap enough to run before every
    /// cursor-relative key.
    ///
    /// This is what makes a decoupled wheel safe: scrolling away from the
    /// cursor and then pressing `d`/`Enter` would otherwise act on a row that
    /// is nowhere on screen.
    pub(crate) fn reanchor_cursor(&mut self) -> bool {
        // An unstamped window (`last_list_rows == 0`) means nothing has been
        // laid out yet; leave the cursor alone rather than snap it to 0.
        if self.last_list_rows == 0 || self.last_visible < self.first_visible {
            return false;
        }
        let anchored = self.cursor.clamp(self.first_visible, self.last_visible);
        let moved = anchored != self.cursor;
        self.cursor = anchored;
        moved
    }

    /// Per-frame reconciliation, run from the loop just before render and ONLY
    /// when something moved (see the guard at the pre-render focus mirror in
    /// `run.rs` — every term there is O(1) and false on a pane-output frame, so
    /// a streaming frame never pays for this).
    ///
    /// Clamps `scroll` to the end of the list, follows the cursor **only while
    /// focused**, and stamps the window cache. Unfocused it merely clamps: that
    /// is what lets `rebuild`'s active-row tracking keep the resting highlight
    /// on the current tab without dragging the viewport with it.
    pub(crate) fn settle_scroll(&mut self, model: &mut FrameModel, rect: Rect) {
        // Mirror the cursor FIRST: `sidebar_geom` reads it off the model, and a
        // key handler that just moved `self.cursor` may not have synced yet —
        // revealing the stale position would scroll to the wrong row.
        self.sync(model);
        let geom = sidebar_geom(model, rect);
        self.scroll = if self.focused {
            scroll_to_reveal(&geom.heights, geom.cursor, geom.list_rows, self.scroll)
        } else {
            self.scroll
                .min(max_sidebar_scroll(&geom.heights, geom.list_rows))
        };
        self.revealed_cursor = geom.cursor;
        self.stamp_window(&geom);
        self.sync(model);
    }

    /// How far PageUp/PageDown steps: one screenful of the window the last
    /// frame laid out. Falls back to the historical fixed 10 before anything
    /// has been laid out (or in a degenerate zero-row window).
    pub(crate) fn page_step(&self) -> usize {
        if self.last_list_rows == 0 {
            10
        } else {
            self.last_list_rows
        }
    }

    /// Record the window the current `scroll` produces, for the O(1) consumers
    /// (`reanchor_cursor`, the PageUp/Down step).
    fn stamp_window(&mut self, geom: &crate::sidebar_view::SidebarGeom) {
        let w = sidebar_window(&geom.heights, geom.list_rows, self.scroll);
        self.first_visible = w.first_visible;
        self.last_visible = w.last_visible;
        self.last_list_rows = geom.list_rows;
    }
}

#[cfg(test)]
#[path = "sidebar_scroll_tests.rs"]
mod tests;
